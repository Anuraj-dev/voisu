//! Local vs Cloud routing: choose the path before constructing dependencies.

mod silent;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use voisu_core::{
    AsrMode, DaemonState, DeliveryOutcome, LifecycleEvidence, LifecycleStage, LocalAsrDiagnostic,
    LocalAsrOutcome, LocalReadiness, Response, Transcript, stop_anchored_timings,
};

use crate::local_model::ActiveReceipt;
#[cfg(not(test))]
use crate::local_model::{
    DevelopmentStore, FileKind, ModelStore, development_local_enabled, development_receipt,
    models_dir, runtime_selection, shipped_catalog,
};
use crate::local_recovery::{RecoveryClock, RecoveryError, RecoveryStore};
pub use crate::local_tail::LocalFormatting;
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
/// A Local worker process that would not exit when asked; it may still hold
/// memory, so Local stays unavailable instead of relaunching beside it.
const LOCAL_UNREAPED: &str = "previous Local worker unreaped; it may still hold memory";

/// Latest Engine choice the daemon has seen, with its persisted revision. A
/// stale reader (older revision) never overwrites a newer commit.
type EngineSelection = Arc<Mutex<Option<(AsrMode, u64)>>>;

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
    /// Cloud selected: passive refresh never launches the worker and an idle
    /// one is unloaded; busy work keeps it until release.
    engine: EngineSelection,
    /// Runs a Cloud unload off the caller's thread once the gate lock is free;
    /// false means nothing was scheduled and the caller drains inline.
    schedule_unload: Box<dyn Fn() -> bool + Send>,
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
    /// When the loaded worker last finished work (or became Ready). The idle
    /// timer unloads it once this is older than the configured timeout.
    idle_since: Option<Instant>,
    /// Unloaded by the idle timer with the receipt kept: Local still reports
    /// ready, and the next admission reloads it while capture runs.
    parked: bool,
    /// The supervisor is checked out to a reload thread. Refresh, release
    /// and the idle timer leave the absent stand-in alone; inference waits.
    loading_in_background: bool,
    /// A reload `admit` started, for its caller to run off the gate lock.
    pending_load: Option<PendingLoad>,
    #[cfg(test)]
    test_clock: Option<Instant>,
    #[cfg(test)]
    test_load_hook: Option<LoadHook>,
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
    /// Runs where a launch verifies and starts the worker process.
    #[cfg(test)]
    test_before_launch: Option<Box<dyn FnOnce() + Send>>,
    /// Replaces `LOAD_WAIT` at Stop.
    #[cfg(test)]
    test_load_wait: Option<Duration>,
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
            engine: Arc::new(Mutex::new(None)),
            schedule_unload: Box::new(|| false),
            admitted_generation: None,
            admitted_recording: None,
            pending_permit: None,
            user_terms: Vec::new(),
            inference_checked_out: false,
            runtime: None,
            requested_device: None,
            worker_failure: None,
            idle_since: None,
            parked: false,
            loading_in_background: false,
            pending_load: None,
            #[cfg(test)]
            test_clock: None,
            #[cfg(test)]
            test_load_hook: None,
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
            #[cfg(test)]
            test_before_launch: None,
            #[cfg(test)]
            test_load_wait: None,
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

    /// The idle timer's clock; tests inject it.
    fn now(&self) -> Instant {
        #[cfg(test)]
        if let Some(now) = self.test_clock {
            return now;
        }
        Instant::now()
    }

    fn release_admission(&mut self) {
        self.admitted_generation = None;
        self.admitted_recording = None;
        if self.loading_in_background {
            // The reload thread restores the supervisor and settles readiness.
            self.readiness = LocalReadiness::Loading;
            self.publish(self.readiness.clone());
            return;
        }
        if !self.inference_checked_out {
            self.readiness = if self.lifecycle.state() == WorkerState::Ready {
                // Finished work restarts the idle timer.
                self.idle_since = Some(self.now());
                LocalReadiness::Ready {
                    model_identity: self.receipt.as_ref().map(local_model_identity),
                }
            } else {
                self.not_ready()
            };
            self.publish(self.readiness.clone());
            // Work admitted before a switch to Cloud has now finished. Release
            // runs on the daemon actor, so the reap (seconds for a stuck
            // worker) happens on the unload thread, under this same lock.
            if self.cloud_unload_due() && !(self.schedule_unload)() {
                self.unload_for_cloud();
            }
        }
    }

    fn cloud_selected(&self) -> bool {
        matches!(
            *self
                .engine
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()),
            Some((AsrMode::Cloud, _))
        )
    }

    /// Records a committed Engine switch; Cloud unloads an idle worker now.
    #[cfg(test)]
    fn select_engine(&mut self, mode: AsrMode, revision: u64) {
        record_engine(&self.engine, mode, revision);
        self.unload_for_cloud();
    }

    /// Cloud is selected and an idle worker is loaded with no admitted,
    /// checked-out or in-flight work holding it.
    fn cloud_unload_due(&self) -> bool {
        self.cloud_selected()
            && self.admitted_generation.is_none()
            && !self.inference_checked_out
            && !self.loading_in_background
            && self.lifecycle.has_child()
            && !matches!(
                self.lifecycle.state(),
                WorkerState::Busy | WorkerState::Stopping
            )
    }

    /// With Cloud selected, unload an idle worker. Admitted, checked-out or
    /// in-flight work is never dropped: `release_admission` schedules this
    /// again once that work is done. Every caller holds the gate lock, the
    /// same lock admission takes, so a drain cannot interleave with an
    /// admission.
    fn unload_for_cloud(&mut self) {
        if self.parked && self.cloud_selected() {
            // Already unloaded by the idle timer: Cloud shows it as Absent.
            self.parked = false;
            self.readiness = LocalReadiness::Absent;
            self.publish(self.readiness.clone());
        }
        if !self.cloud_unload_due() {
            return;
        }
        // The receipt stays, so a switch back to Local relaunches the same
        // model on the next refresh or admission. Only a child confirmed gone
        // is Absent: a survivor may still hold VRAM.
        self.readiness = match self.lifecycle.drain_for_cloud_selection() {
            Ok(_) if !self.lifecycle.has_child() => LocalReadiness::Absent,
            result => {
                self.worker_failure = Some(match result {
                    Err(error) => format!("{LOCAL_UNREAPED}: {error}"),
                    Ok(_) => LOCAL_UNREAPED.to_owned(),
                });
                self.lifecycle.mark_unavailable();
                self.not_ready()
            }
        };
        self.publish(self.readiness.clone());
    }

    /// Cloud was selected while this refresh verified the model: launch
    /// nothing. The receipt stays for a later switch back to Local.
    fn cloud_preempts_launch(&mut self) -> bool {
        if !self.cloud_selected() {
            return false;
        }
        self.readiness = LocalReadiness::Absent;
        true
    }

    fn admit(&mut self, kind_ready_required: bool) -> LocalAdmission {
        if self.loading_in_background && self.admitted_generation.is_none() {
            // A Start abandoned during a reload left the load running: this
            // Start joins it, and its Stop waits on the same load.
            return match self.receipt.clone() {
                Some(receipt) if !self.cloud_selected() => {
                    self.admitted_generation = Some(self.lifecycle.generation());
                    self.readiness = LocalReadiness::Busy;
                    self.publish(self.readiness.clone());
                    LocalAdmission::Ready {
                        model_identity: local_model_identity(&receipt),
                        receipt,
                    }
                }
                _ => LocalAdmission::Unavailable {
                    error: LOCAL_NOT_READY.to_owned(),
                },
            };
        }
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
                if self.parked && !self.begin_reload(&receipt) {
                    return LocalAdmission::Unavailable {
                        error: match &self.readiness {
                            LocalReadiness::Unavailable { error } => error.clone(),
                            _ => LOCAL_NOT_READY.to_owned(),
                        },
                    };
                }
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
        if self.cloud_selected() {
            // A refresh queued while Local was selected may run after the
            // switch: Cloud never loads the Local worker, it only unloads one.
            self.unload_for_cloud();
            return self.readiness.clone();
        }
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
            && !self.loading_in_background
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
        if self.loading_in_background && !self.inference_checked_out && !self.cloud_selected() {
            // A Start abandoned during a reload left the load running with no
            // admission: the next Start may join it (see `admit`), so the
            // daemon's pre-capture check must see Ready, not Loading.
            if let Some(receipt) = &self.receipt {
                return LocalReadiness::Ready {
                    model_identity: Some(local_model_identity(receipt)),
                };
            }
        }
        if self.inference_checked_out || self.loading_in_background {
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
            if self.cloud_preempts_launch() {
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
        } else if self.parked
            && self.receipt.is_some()
            && !self.cloud_selected()
            && self.lifecycle.can_launch_replacement()
        {
            // Unloaded after the idle timeout, same model still selected:
            // ready to record. Admission reloads it while the user speaks.
            self.readiness = LocalReadiness::Ready {
                model_identity: self.receipt.as_ref().map(local_model_identity),
            };
            return self.readiness.clone();
        } else if self.lifecycle.has_child() {
            // A child that is not Ready and was never reaped (verification
            // above clears the older failure detail).
            self.worker_failure
                .get_or_insert_with(|| LOCAL_UNREAPED.to_owned());
            self.lifecycle.mark_unavailable();
            self.readiness = self.not_ready();
            return self.readiness.clone();
        } else if self.lifecycle.can_launch_replacement() {
            if self.cloud_preempts_launch() {
                return self.readiness.clone();
            }
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

    #[cfg(test)]
    fn verify_consented(
        &self,
    ) -> Result<crate::local_model::VerifiedConsentedInstall, crate::local_model::ConsentedError>
    {
        self.verify_consented_with_progress(|| {})
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
            crate::local_worker::verify_consented_install_refreshing(&store, on_verifying)
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
        if !self.attach_selected_worker(selected) {
            return false;
        }
        let prepared = self.lifecycle.prepare("l4-prepare", Instant::now());
        if let Err(error) = &prepared {
            self.worker_failure = Some(error.to_string());
        }
        let ready = prepared.is_ok() && self.lifecycle.state() == WorkerState::Ready;
        if ready {
            self.capture_runtime_identity(selected);
            self.idle_since = Some(self.now());
        }
        ready
    }

    /// Verify, launch and attach the selected worker; it still owes `Prepare`.
    fn attach_selected_worker(&mut self, selected: &ActiveReceipt) -> bool {
        self.worker_failure = None;
        self.parked = false;
        if !self.lifecycle.can_launch_replacement() {
            return false;
        }
        self.publish(LocalReadiness::Loading);
        let Some(worker) = self.spawn_worker(selected) else {
            return false;
        };
        self.attach_worker(selected, worker)
    }

    fn attach_worker(&mut self, selected: &ActiveReceipt, worker: GateWorker) -> bool {
        self.lifecycle
            .pin_model_receipt_hash(selected.receipt_hash.clone());
        let attached = self
            .lifecycle
            .attach_replacement(worker, Instant::now())
            .is_ok();
        #[cfg(test)]
        if attached {
            let generation = self.lifecycle.generation();
            let hash = selected.receipt_hash.clone();
            if let Some(child) = self.lifecycle.child_mut() {
                child.generation = generation;
                child.model_receipt_hash = hash;
            }
        }
        attached
    }

    /// Recording start with the worker idle-unloaded: only flip state here.
    /// Verify (a full weights hash), launch and `Prepare` (up to
    /// `LOAD_DEADLINE`) all run on the load thread, so capture starts at once
    /// and the load overlaps speech. A failure there leaves Local unavailable
    /// and the Recording fails closed at Stop. Cloud selected launches
    /// nothing.
    fn begin_reload(&mut self, selected: &ActiveReceipt) -> bool {
        self.parked = false;
        if self.cloud_preempts_launch() {
            self.publish(self.readiness.clone());
            return false;
        }
        self.worker_failure = None;
        if !self.lifecycle.can_launch_replacement() {
            self.lifecycle.mark_unavailable();
            self.readiness = self.not_ready();
            self.publish(self.readiness.clone());
            return false;
        }
        self.loading_in_background = true;
        self.pending_load = Some(PendingLoad {
            launch: self.launch_plan(selected),
            #[cfg(test)]
            hook: self.test_load_hook.take(),
        });
        true
    }

    /// The load thread launched the worker (or failed to): attach it and
    /// check its supervisor out for `Prepare`. `None` means the load ended.
    fn attach_launched(
        &mut self,
        launch: WorkerLaunch,
        result: Result<GateWorker, Option<String>>,
    ) -> Option<PreparingLoad> {
        let selected = launch.selected.clone();
        let attached = self
            .settle_launch(launch, result)
            .is_some_and(|worker| self.attach_worker(&selected, worker));
        if !attached {
            self.fail_load(None);
            return None;
        }
        if self.admitted_generation.is_some() {
            // The Recording admitted before the launch now pins this worker.
            self.admitted_generation = Some(self.lifecycle.generation());
        }
        let correlation = self.lifecycle.prepare_correlation("l4-prepare");
        Some(PreparingLoad {
            supervisor: self.lifecycle.take_supervisor(),
            correlation,
            selected,
        })
    }

    /// The load thread is done: return its supervisor and settle readiness.
    /// A failed or overdue load leaves Local unavailable, so the waiting
    /// Recording fails closed; nothing falls back to Cloud.
    fn finish_load(&mut self, done: FinishedLoad) {
        self.lifecycle.restore_supervisor(done.supervisor);
        match done.result {
            Ok(()) if self.lifecycle.state() == WorkerState::Ready => {
                self.loading_in_background = false;
                self.capture_runtime_identity(&done.selected);
                self.idle_since = Some(self.now());
                if self.admitted_generation.is_none() {
                    // The Recording was abandoned during the load; Cloud
                    // selected in the meantime unloads it here.
                    self.release_admission();
                }
            }
            Ok(()) => self.fail_load(Some("Local worker did not become ready".to_owned())),
            Err(error) => self.fail_load(Some(error)),
        }
    }

    fn fail_load(&mut self, failure: Option<String>) {
        self.loading_in_background = false;
        if failure.is_some() {
            self.worker_failure = failure;
        }
        self.lifecycle.mark_unavailable();
        self.readiness = self.not_ready();
        self.publish(self.readiness.clone());
        if self.admitted_generation.is_none() {
            self.release_admission();
        }
    }

    /// The idle timer: unload a Ready worker no work has used for `timeout`
    /// (`None` = never). Admitted, checked-out or loading work is never
    /// dropped, and Cloud selection owns its own unload. Returns how long
    /// until this worker is due, or `None` when no idle worker waits.
    fn unload_if_idle(&mut self, now: Instant, timeout: Option<Duration>) -> Option<Duration> {
        let idle = self.lifecycle.has_child()
            && self.lifecycle.state() == WorkerState::Ready
            && self.admitted_generation.is_none()
            && !self.inference_checked_out
            && !self.loading_in_background
            && !self.cloud_selected();
        if !idle {
            return None;
        }
        let timeout = timeout?;
        let due = *self.idle_since.get_or_insert(now) + timeout;
        if now < due {
            return Some(due - now);
        }
        self.idle_since = None;
        // A confirmed exit keeps the receipt and still reports ready; only a
        // process that survived the reap (it may hold VRAM) is unavailable.
        self.readiness = match self.lifecycle.unload_idle() {
            Ok(_) if !self.lifecycle.has_child() => {
                self.parked = true;
                LocalReadiness::Ready {
                    model_identity: self.receipt.as_ref().map(local_model_identity),
                }
            }
            result => {
                self.worker_failure = Some(match result {
                    Err(error) => format!("{LOCAL_UNREAPED}: {error}"),
                    Ok(_) => LOCAL_UNREAPED.to_owned(),
                });
                self.lifecycle.mark_unavailable();
                self.not_ready()
            }
        };
        self.publish(self.readiness.clone());
        None
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
        let mut launch = self.launch_plan(selected);
        let result = launch.run();
        self.settle_launch(launch, result)
    }

    /// What a launch needs, taken out of the gate so it can run unlocked.
    fn launch_plan(&mut self, selected: &ActiveReceipt) -> WorkerLaunch {
        WorkerLaunch {
            selected: selected.clone(),
            #[cfg(test)]
            consented: self.test_consented.take(),
            #[cfg(test)]
            next_worker: self.test_next_worker.take(),
            #[cfg(test)]
            before: self.test_before_launch.take(),
            #[cfg(test)]
            launched: false,
            #[cfg(test)]
            verified: false,
        }
    }

    /// Return a launch's borrowed test state and record why it failed.
    fn settle_launch(
        &mut self,
        launch: WorkerLaunch,
        result: Result<GateWorker, Option<String>>,
    ) -> Option<GateWorker> {
        #[cfg(test)]
        {
            if self.test_consented.is_none() {
                self.test_consented = launch.consented;
            }
            if self.test_next_worker.is_none() {
                self.test_next_worker = launch.next_worker;
            }
            if launch.launched {
                self.launch_count = self.launch_count.saturating_add(1);
            }
            if launch.verified {
                self.verification_count
                    .set(self.verification_count.get() + 1);
            }
        }
        #[cfg(not(test))]
        drop(launch);
        match result {
            Ok(worker) => Some(worker),
            Err(failure) => {
                if failure.is_some() {
                    self.worker_failure = failure;
                }
                None
            }
        }
    }

    #[cfg(test)]
    fn attach_ready_worker(&mut self, hash: String, text: &str, accepts_prompt: bool) {
        let mut supervisor = WorkerSupervisor::absent();
        let worker = FakeWorker {
            scripted_text: Some(text.to_owned()),
            generation: 1,
            model_receipt_hash: hash.clone(),
            accepts_prompt,
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

#[cfg(test)]
type LoadHook = Box<dyn FnOnce(&mut WorkerSupervisor<GateWorker>) + Send>;

/// One worker launch: re-verify a consented install, then start the process.
/// Verification hashes the full weights (seconds), so a reload runs this on
/// its load thread with the gate lock free.
struct WorkerLaunch {
    selected: ActiveReceipt,
    #[cfg(test)]
    consented: Option<(
        crate::local_model::ModelStore,
        crate::local_model::ConsentedCatalogEntry,
    )>,
    #[cfg(test)]
    next_worker: Option<FakeWorker>,
    #[cfg(test)]
    before: Option<Box<dyn FnOnce() + Send>>,
    #[cfg(test)]
    launched: bool,
    #[cfg(test)]
    verified: bool,
}

impl WorkerLaunch {
    /// `Err(Some(reason))` explains a failure; `Err(None)` has no detail.
    fn run(&mut self) -> Result<GateWorker, Option<String>> {
        #[cfg(test)]
        if let Some(before) = self.before.take() {
            before();
        }
        if !crate::local_model::development_local_enabled() && is_consented_receipt(&self.selected)
        {
            // This exact re-verification and receipt comparison also runs in
            // tests. Only process creation is replaced by the scripted worker.
            let install = match self.verify() {
                Ok(install)
                    if crate::local_worker::consented_active_receipt(&install) == self.selected =>
                {
                    install
                }
                _ => return Err(Some(LOCAL_UNAVAILABLE.into())),
            };
            #[cfg(not(test))]
            {
                return crate::local_worker::launch_consented_worker(install)
                    .map(|worker| RuntimeWorker::Consented(Box::new(worker)))
                    .map_err(|error| {
                        Some(match error {
                            crate::local_model::ConsentedError::Health(detail) => detail,
                            crate::local_model::ConsentedError::GpuBusy => {
                                crate::local_worker::GPU_BUSY.to_owned()
                            }
                            error => format!("{error:?}"),
                        })
                    });
            }
            #[cfg(test)]
            drop(install);
        }
        #[cfg(test)]
        {
            self.launched = true;
            self.next_worker.take().ok_or(None)
        }
        #[cfg(not(test))]
        launch_unconsented(&self.selected)
    }

    fn verify(
        &mut self,
    ) -> Result<crate::local_model::VerifiedConsentedInstall, crate::local_model::ConsentedError>
    {
        #[cfg(test)]
        {
            self.verified = true;
            let (store, entry) = self
                .consented
                .as_ref()
                .ok_or(crate::local_model::ConsentedError::Missing)?;
            crate::local_model::consented::verify_with_entry_notifying(store, entry, || {})
        }
        #[cfg(not(test))]
        {
            crate::local_worker::verify_default_consented_install()
        }
    }
}

/// A development or shipped whisper.cpp worker.
#[cfg(not(test))]
fn launch_unconsented(selected: &ActiveReceipt) -> Result<GateWorker, Option<String>> {
    if development_local_enabled()
        && let Ok((manifest, handle)) =
            DevelopmentStore::open_default().and_then(|store| store.selected())
        && selected.catalog_id == manifest.model_id
        && selected.artifact_id == handle.manifest_hash
    {
        return crate::local_worker::DevWorker::launch(
            manifest,
            handle.manifest_hash,
            &handle.path,
        )
        .map(|worker| RuntimeWorker::Development(Box::new(worker)))
        .map_err(|error| Some(error.to_string()));
    }
    launch_shipped_whisper(selected).ok_or(None)
}

#[cfg(not(test))]
fn launch_shipped_whisper(selected: &ActiveReceipt) -> Option<GateWorker> {
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

/// A reload `admit` started, for its caller to run off the gate lock.
struct PendingLoad {
    launch: WorkerLaunch,
    #[cfg(test)]
    hook: Option<LoadHook>,
}

/// An attached worker whose supervisor is checked out for `Prepare`.
struct PreparingLoad {
    supervisor: WorkerSupervisor<GateWorker>,
    correlation: Correlation,
    selected: ActiveReceipt,
}

struct FinishedLoad {
    supervisor: WorkerSupervisor<GateWorker>,
    selected: ActiveReceipt,
    result: Result<(), String>,
}

/// Notified after a reload finishes, once the gate lock is released.
fn load_finished() -> &'static Condvar {
    static DONE: Condvar = Condvar::new();
    &DONE
}

/// How long a Stop waits for a reload: the load's own bound plus margin, so
/// a stuck load thread still fails closed instead of hanging the Recording.
pub(crate) const LOAD_WAIT: Duration = Duration::from_secs(65);

fn load_wait(gate: &Gate) -> Duration {
    #[cfg(test)]
    if let Some(wait) = gate.test_load_wait {
        return wait;
    }
    let _ = gate;
    LOAD_WAIT
}

#[cfg(test)]
static LOAD_WAITS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Runs a reload `admit` started on its own thread. If the thread cannot
/// start, the load runs here, which only delays this Start as before.
fn spawn_background_load(load: PendingLoad) {
    let (send, receive) = std::sync::mpsc::channel::<PendingLoad>();
    let spawned = std::thread::Builder::new()
        .name("local-load".into())
        .spawn(move || {
            if let Ok(load) = receive.recv() {
                run_background_load(load);
            }
        });
    match spawned {
        Ok(handle) => {
            #[cfg(test)]
            background_loads()
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .push(handle);
            #[cfg(not(test))]
            drop(handle);
            if let Err(std::sync::mpsc::SendError(load)) = send.send(load) {
                run_background_load(load);
            }
        }
        Err(error) => {
            eprintln!("Local worker load thread failed to start ({error}); loading inline");
            run_background_load(load);
        }
    }
}

/// Launch, attach and load the worker, taking the gate lock only to attach
/// and to settle. `Prepare` is bounded by the supervisor's `LOAD_DEADLINE`.
/// A panic keeps the supervisor (and its restart budget) and fails the load.
fn run_background_load(load: PendingLoad) {
    let PendingLoad {
        mut launch,
        #[cfg(test)]
        hook,
    } = load;
    let launched = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| launch.run()))
        .unwrap_or_else(|_| {
            eprintln!("Local worker launch panicked; Local is unavailable");
            Err(Some("Local worker launch failed".to_owned()))
        });
    let preparing = match gate().lock() {
        Ok(mut gate) => gate.attach_launched(launch, launched),
        Err(poison) => {
            publish_poisoned_gate(poison);
            None
        }
    };
    if let Some(mut preparing) = preparing {
        let correlation = preparing.correlation.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let result = preparing
                .supervisor
                .prepare(correlation, Instant::now())
                .map(|_| ())
                .map_err(|error| error.to_string());
            #[cfg(test)]
            if let Some(hook) = hook {
                hook(&mut preparing.supervisor);
            }
            result
        }))
        .unwrap_or_else(|_| {
            eprintln!("Local worker load panicked; Local is unavailable");
            Err("Local worker load failed".to_owned())
        });
        match gate().lock() {
            Ok(mut gate) => gate.finish_load(FinishedLoad {
                supervisor: preparing.supervisor,
                selected: preparing.selected,
                result,
            }),
            Err(poison) => publish_poisoned_gate(poison),
        }
    }
    load_finished().notify_all();
}

#[cfg(test)]
fn background_loads() -> &'static Mutex<Vec<std::thread::JoinHandle<()>>> {
    static HANDLES: Mutex<Vec<std::thread::JoinHandle<()>>> = Mutex::new(Vec::new());
    &HANDLES
}

/// Test-only: wait for every reload thread. Call without the gate.
#[cfg(test)]
pub fn join_background_loads_for_tests() {
    let handles = std::mem::take(
        &mut *background_loads()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()),
    );
    for handle in handles {
        handle.join().expect("Local load thread");
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
    if gate.loading_in_background {
        // The worker reloaded at Start is still loading: wait for it.
        #[cfg(test)]
        LOAD_WAITS.fetch_add(1, Ordering::SeqCst);
        let wait = load_wait(&gate);
        gate = load_finished()
            .wait_timeout_while(gate, wait, |gate| gate.loading_in_background)
            .map_err(|_| LocalError::Unavailable)?
            .0;
    }
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
    no_text_outcome("silence", observed_device)
}

fn no_text_outcome(reason: &str, observed_device: &str) -> Result<WorkerOutcome, LocalError> {
    if observed_device.is_empty() || observed_device == "unknown" {
        return Err(LocalError::Inference(format!(
            "{reason} inference lacked an observed device"
        )));
    }
    Ok(WorkerOutcome::NoText {
        reason: reason.to_owned(),
        observed_device: observed_device.to_owned(),
        metrics: crate::local_worker::InferenceMetrics::default(),
    })
}

/// Run the speech check on a Recording. Digital silence skips it (`None`):
/// the existing silence path already names it "silence". Every result is
/// logged, pass or refusal, so a wrong refusal can be traced to its score.
fn speech_check(pcm: &[u8]) -> Option<crate::local_vad::SpeechCheck> {
    if is_silence_pcm(pcm) {
        return None;
    }
    let started = Instant::now();
    let check = crate::local_vad::check_speech(pcm);
    eprintln!(
        "Local speech check: {} (peak {:.3} over {} windows, gain {:.1}, {} ms)",
        if check.has_speech {
            "speech"
        } else {
            "no speech"
        },
        check.peak_probability,
        check.windows_scored,
        check.gain,
        started.elapsed().as_millis()
    );
    Some(check)
}

/// Refuse a Recording the speech check heard no speech in, so no Local
/// model can turn room noise into "you" or "Okay.". `None` sends it to the
/// model unchanged, as does a worker whose device is not yet known.
fn no_speech_outcome(
    check: Option<crate::local_vad::SpeechCheck>,
    observed_device: &str,
) -> Option<WorkerOutcome> {
    match check {
        Some(check) if !check.has_speech => no_text_outcome("no speech", observed_device).ok(),
        _ => None,
    }
}

/// Put the speech check's score on the Recording's Local ASR history entry.
fn record_speech_check(
    diagnostic: &mut LocalAsrDiagnostic,
    check: Option<crate::local_vad::SpeechCheck>,
) {
    if let Some(check) = check {
        diagnostic.speech_peak_permille = Some(scaled_u16(check.peak_probability, 1000.0));
        diagnostic.speech_gain_percent = Some(scaled_u16(check.gain, 100.0));
    }
}

fn scaled_u16(value: f32, scale: f32) -> u16 {
    // `as` saturates and maps NaN to 0, which is what a history field wants.
    (value * scale).round() as u16
}

fn infer_unlocked(
    session: &mut InferenceSession,
    recording_id: &str,
    request_id: &str,
    pcm: Vec<u8>,
    user_terms: &[String],
) -> Result<WorkerOutcome, LocalError> {
    let correlation = correlation(&session.receipt_hash, recording_id, request_id);
    // The supervisor drops the prompt unless this worker advertised support,
    // so Parakeet and an old worker are sent exactly what they always were.
    let prompt = crate::dictionary::local_prompt_for_terms(
        user_terms,
        voisu_core::is_explicit_english(&session.language),
    );
    transcribe_through_supervisor(
        &mut session.supervisor,
        correlation,
        pcm,
        prompt,
        &session.sentinel,
    )
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
    model_identity_on(receipt, crate::config::local_device)
}

/// A Local model tier reads as `Accurate (distil-large-v3.5, GPU)`, its
/// device being where the worker runs (`device`, read only for a tier). Other
/// installs keep their catalog id; the id itself is for `voisu doctor`.
fn model_identity_on(
    receipt: &ActiveReceipt,
    device: impl FnOnce() -> crate::config::LocalDevice,
) -> String {
    if is_consented_receipt(receipt)
        && let Some(tier) = crate::local_model::LocalTier::of_entry_id(&receipt.catalog_id)
    {
        return tier.label(crate::local_model::runs_on_gpu(device()));
    }
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
        gate.engine = engine_selection().clone();
        gate.schedule_unload = Box::new(spawn_cloud_unload);
        #[cfg(not(test))]
        spawn_idle_unload_timer();
        Mutex::new(gate)
    })
}

/// The idle-unload timer: one daemon thread that wakes at most every 30 s
/// (sooner when a worker is nearly due) and unloads an idle worker. The
/// setting is read each pass outside the gate lock, so a change applies
/// within 30 s without a restart. The thread sleeps first, so it never
/// touches the gate before `gate()` finishes initializing.
#[cfg(not(test))]
fn spawn_idle_unload_timer() {
    const POLL: Duration = Duration::from_secs(30);
    let spawned = std::thread::Builder::new()
        .name("local-idle-unload".into())
        .spawn(|| {
            let mut wait = POLL;
            loop {
                std::thread::sleep(wait);
                let timeout = crate::config::local_idle_unload();
                let mut gate = match gate().lock() {
                    Ok(gate) => gate,
                    Err(poison) => {
                        // Worker ownership is uncertain after a panic; the
                        // gate stays poisoned and fails closed.
                        eprintln!("Local idle-unload timer stopped: the Local gate panicked");
                        publish_poisoned_gate(poison);
                        return;
                    }
                };
                wait = gate
                    .unload_if_idle(Instant::now(), timeout)
                    .map_or(POLL, |due| due.clamp(Duration::from_secs(1), POLL));
            }
        });
    if let Err(error) = spawned {
        eprintln!("Local idle-unload timer failed to start ({error}); idle workers stay loaded");
    }
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

fn engine_selection() -> &'static EngineSelection {
    static ENGINE: OnceLock<EngineSelection> = OnceLock::new();
    ENGINE.get_or_init(|| Arc::new(Mutex::new(None)))
}

fn record_engine(slot: &Mutex<Option<(AsrMode, u64)>>, mode: AsrMode, revision: u64) {
    let mut seen = slot.lock().unwrap_or_else(|poison| poison.into_inner());
    if seen.is_none_or(|(_, latest)| revision >= latest) {
        *seen = Some((mode, revision));
    }
}

/// The Engine choice as just read from disk (status or admission). Never
/// blocks on the gate, so status polls stay cheap.
pub fn note_engine_selection(mode: AsrMode, revision: u64) {
    record_engine(engine_selection(), mode, revision);
}

/// A committed Engine switch. Cloud unloads an idle Local worker; busy work
/// keeps it until that work is released. The choice is recorded before this
/// returns; the unload waits for the gate on its own thread so the switch
/// reply never waits behind a model verification. A late unload cannot undo
/// a newer switch back to Local: it rechecks the revision-ordered choice
/// under the gate lock.
pub fn apply_engine_selection(mode: AsrMode, revision: u64) {
    note_engine_selection(mode, revision);
    if mode == AsrMode::Cloud && !spawn_cloud_unload() {
        // No thread: drain here (the persist queue, not the daemon actor).
        match gate().lock() {
            Ok(mut gate) => gate.unload_for_cloud(),
            Err(poison) => publish_poisoned_gate(poison),
        }
    }
}

/// Unloads an idle Local worker on its own thread once the global gate is
/// free. False when the thread could not start; the caller drains inline.
fn spawn_cloud_unload() -> bool {
    let spawned = std::thread::Builder::new()
        .name("local-cloud-unload".into())
        .spawn(|| match gate().lock() {
            Ok(mut gate) => gate.unload_for_cloud(),
            Err(poison) => publish_poisoned_gate(poison),
        });
    match spawned {
        Ok(handle) => {
            #[cfg(test)]
            cloud_unloads()
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .push(handle);
            #[cfg(not(test))]
            drop(handle);
            true
        }
        Err(error) => {
            eprintln!("Local worker unload thread failed to start ({error}); unloading inline");
            false
        }
    }
}

#[cfg(test)]
fn cloud_unloads() -> &'static Mutex<Vec<std::thread::JoinHandle<()>>> {
    static HANDLES: Mutex<Vec<std::thread::JoinHandle<()>>> = Mutex::new(Vec::new());
    &HANDLES
}

/// Test-only: wait for every scheduled Cloud unload. Call without the gate.
#[cfg(test)]
pub fn join_cloud_unloads_for_tests() {
    loop {
        let handles = std::mem::take(
            &mut *cloud_unloads()
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()),
        );
        if handles.is_empty() {
            return;
        }
        for handle in handles {
            handle.join().expect("Cloud unload thread");
        }
    }
}

/// Test-only: whether the global gate holds a Local worker process.
#[cfg(test)]
pub fn local_worker_loaded_for_tests() -> bool {
    gate().lock().expect("gate").lifecycle.has_child()
}

/// Test-only Engine revisions start above this. Unrelated tests read their own
/// temp configs (small revisions) in parallel; the revision order keeps those
/// reads out of the shared slot during a gate test session.
#[cfg(test)]
pub const TEST_ENGINE_REVISION: u64 = 1 << 40;

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
    let (admission, load) = match gate().lock() {
        Ok(mut gate) => {
            let admission = gate.admit(kind_ready_required);
            (admission, gate.pending_load.take())
        }
        Err(poison) => {
            publish_poisoned_gate(poison);
            return LocalAdmission::Unavailable {
                error: LOCAL_NOT_READY.to_owned(),
            };
        }
    };
    if let Some(load) = load {
        spawn_background_load(load);
    }
    admission
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

fn admitted_receipt_hash() -> Option<String> {
    let gate = gate().lock().ok()?;
    gate.receipt
        .as_ref()
        .map(|receipt| receipt.receipt_hash.clone())
}

#[cfg(test)]
static TEST_RECOVERY_DIR: Mutex<Option<std::path::PathBuf>> = Mutex::new(None);

/// The recovery store a Local Recording persists to (a temp dir in tests).
fn open_recovery() -> Result<RecoveryStore, RecoveryError> {
    #[cfg(test)]
    if let Some(dir) = TEST_RECOVERY_DIR
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone()
    {
        return RecoveryStore::open(dir, RecoveryClock::system());
    }
    RecoveryStore::open_default(RecoveryClock::system())
}

/// Finish a Local Recording: optional recovery persist, inference, tail, Delivery.
pub fn complete_recording(
    recording_id: &str,
    pcm: Vec<u8>,
    user_terms: &[String],
    formatting: LocalFormatting,
    recovery_enabled: bool,
    cloud_selected: bool,
) -> Result<LocalCompletion, LocalError> {
    if cloud_selected {
        return Err(LocalError::CloudSelected);
    }
    // Persist before waiting on a reload, so a failed or overdue load still
    // leaves the audio to recover. The receipt is fixed through the load.
    let persisted = recovery_enabled
        && match admitted_receipt_hash() {
            Some(receipt_hash) => {
                let persist = open_recovery().and_then(|store| {
                    store.persist_before_inference(recording_id, &pcm, &receipt_hash)
                });
                if let Err(error) = persist {
                    if let Ok(mut gate) = gate().lock() {
                        gate.release_admission();
                    }
                    return Err(LocalError::Recovery(error));
                }
                true
            }
            None => false,
        };
    let mut session = match take_inference_session() {
        Ok(session) => session,
        Err(error) => {
            if persisted {
                let _ = open_recovery().and_then(|store| store.mark_failed(recording_id));
            }
            return Err(error);
        }
    };
    assert_gate_unlocked();
    let request_id = format!("{recording_id}-asr");
    // Replay stays ungated: it is an explicit request to transcribe again.
    let speech = speech_check(&pcm);
    let inferred = match no_speech_outcome(speech, session.supervisor.observed_device()) {
        Some(outcome) => Ok(outcome),
        None => infer_unlocked(
            &mut session,
            recording_id,
            &request_id,
            pcm.clone(),
            user_terms,
        ),
    };
    let outcome = match inferred {
        Ok(outcome) => outcome,
        Err(_) if is_silence_pcm(&pcm) => {
            match silence_outcome(session.supervisor.observed_device()) {
                Ok(outcome) => outcome,
                Err(silence_error) => {
                    restore_inference_session(session);
                    if recovery_enabled {
                        let _ = open_recovery().and_then(|store| store.mark_failed(recording_id));
                    }
                    return Err(silence_error);
                }
            }
        }
        Err(error) => {
            restore_inference_session(session);
            if recovery_enabled {
                let _ = open_recovery().and_then(|store| store.mark_failed(recording_id));
            }
            return Err(error);
        }
    };
    let mut local_asr = local_asr_from_session(&session, &outcome);
    record_speech_check(&mut local_asr, speech);
    let decision = decide_transcript(
        recording_id,
        &pcm,
        outcome,
        user_terms,
        formatting.for_model_language(&session.language),
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
    formatting: LocalFormatting,
    cloud_selected: bool,
    from_recovery: bool,
) -> Result<TerminalDecision, LocalError> {
    if cloud_selected && from_recovery {
        return Err(LocalError::CloudSelected);
    }
    let mut session = take_inference_session()?;
    assert_gate_unlocked();
    let request_id = format!("{recording_id}-replay");
    let outcome = match infer_unlocked(
        &mut session,
        recording_id,
        &request_id,
        pcm.clone(),
        user_terms,
    ) {
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
        formatting.for_model_language(&session.language),
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
    formatting: LocalFormatting,
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
        formatting,
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
    formatting: LocalFormatting,
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
        formatting,
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
        speech_peak_permille: None,
        speech_gain_percent: None,
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
        speech_peak_permille: None,
        speech_gain_percent: None,
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
    inject_ready_with_prompt_support(receipt, text, false);
}

/// Test-only: like [`inject_ready`], with a worker whose Ready does or does
/// not advertise prompt support.
#[cfg(test)]
pub fn inject_ready_with_prompt_support(receipt: ActiveReceipt, text: &str, accepts_prompt: bool) {
    let mut gate = gate().lock().expect("gate");
    let hash = receipt.receipt_hash.clone();
    gate.receipt = Some(receipt.clone());
    gate.attach_ready_worker(hash, text, accepts_prompt);
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
    // A previous test's unload or reload must not reach this test's worker.
    join_cloud_unloads_for_tests();
    join_background_loads_for_tests();
    *TEST_RECOVERY_DIR
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = None;
    if let Ok(mut gate) = gate().lock() {
        *gate = Gate::new();
        gate.published = published_readiness().clone();
        gate.published_sentinel = published_sentinel().clone();
        // The production slot and scheduler. Seeded Local above every
        // revision a parallel test's temp config can reach.
        gate.engine = engine_selection().clone();
        *gate
            .engine
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) =
            Some((AsrMode::Local, TEST_ENGINE_REVISION));
        gate.schedule_unload = Box::new(spawn_cloud_unload);
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

    const LITERAL: LocalFormatting =
        LocalFormatting::new(crate::config::WritingMode::Literal, None);

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

    fn next_consented_worker(receipt: &ActiveReceipt) -> FakeWorker {
        FakeWorker {
            model_receipt_hash: receipt.receipt_hash.clone(),
            observed_device: "cuda:0".into(),
            scripted_text: Some("still delivered".into()),
            ..Default::default()
        }
    }

    #[test]
    fn passive_refresh_with_cloud_selected_launches_nothing() {
        let (_state, mut gate, _) = consented_gate();
        // Status read Cloud; an older Local read arriving late cannot undo it.
        record_engine(&gate.engine, AsrMode::Cloud, 2);
        record_engine(&gate.engine, AsrMode::Local, 1);
        for _ in 0..3 {
            assert!(!gate.refresh_passive().admits_capture());
        }
        assert_eq!(gate.launch_count, 0);
        assert_eq!(
            gate.verification_count.get(),
            0,
            "Cloud must not hash models"
        );
        assert!(!gate.lifecycle.has_child());
        // Switching back to Local loads on the next refresh as before.
        record_engine(&gate.engine, AsrMode::Local, 3);
        assert!(matches!(
            gate.refresh_passive(),
            LocalReadiness::Ready { .. }
        ));
        assert_eq!(gate.launch_count, 1);
    }

    #[test]
    fn switch_to_cloud_unloads_an_idle_worker() {
        let (_state, mut gate, receipt) = consented_gate();
        assert!(matches!(gate.refresh(), LocalReadiness::Ready { .. }));
        assert!(gate.lifecycle.has_child());
        gate.select_engine(AsrMode::Cloud, 1);
        assert!(!gate.lifecycle.has_child());
        assert_eq!(gate.lifecycle.state(), WorkerState::Absent);
        assert_eq!(published_snapshot(&gate.published), LocalReadiness::Absent);
        assert_eq!(gate.refresh_passive(), LocalReadiness::Absent);
        assert_eq!(gate.launch_count, 1);
        // Back to Local: the same receipt relaunches.
        gate.select_engine(AsrMode::Local, 2);
        gate.test_next_worker = Some(next_consented_worker(&receipt));
        assert!(matches!(gate.refresh(), LocalReadiness::Ready { .. }));
        assert_eq!(gate.launch_count, 2);
        assert!(gate.lifecycle.has_child());
    }

    #[test]
    fn an_unreaped_cloud_unload_is_unavailable_with_a_reason() {
        let (_state, mut gate, receipt) = consented_gate();
        gate.test_next_worker = Some(FakeWorker {
            reap: crate::local_worker::ReapOutcome::Unreaped,
            ..next_consented_worker(&receipt)
        });
        assert!(matches!(gate.refresh(), LocalReadiness::Ready { .. }));
        gate.select_engine(AsrMode::Cloud, 1);
        // The process survived: it may still hold VRAM, so never "Absent".
        assert!(gate.lifecycle.has_child());
        let published = published_snapshot(&gate.published);
        assert!(
            matches!(&published, LocalReadiness::Unavailable { error } if error.contains("unreaped")),
            "{published:?}"
        );
        // Back to Local: the surviving process blocks a relaunch and says why.
        gate.select_engine(AsrMode::Local, 2);
        let readiness = gate.refresh();
        assert!(
            matches!(&readiness, LocalReadiness::Unavailable { error } if error.contains("unreaped")),
            "{readiness:?}"
        );
        assert_eq!(gate.launch_count, 1);
    }

    #[test]
    fn clean_engine_cycles_never_exhaust_the_restart_budget() {
        let (_state, mut gate, receipt) = consented_gate();
        let cycles = crate::local_worker::MAX_RESTARTS + 2;
        for cycle in 0..cycles {
            let revision = 2 * cycle as u64;
            gate.select_engine(AsrMode::Local, revision);
            gate.test_next_worker = Some(next_consented_worker(&receipt));
            let readiness = gate.refresh_passive();
            assert!(
                matches!(readiness, LocalReadiness::Ready { .. }),
                "cycle {cycle}: {readiness:?}"
            );
            gate.select_engine(AsrMode::Cloud, revision + 1);
            assert!(!gate.lifecycle.has_child(), "cycle {cycle}");
        }
        assert_eq!(gate.launch_count as usize, cycles);
    }

    #[test]
    fn cloud_selected_while_a_refresh_verifies_launches_nothing() {
        let (_state, mut gate, _) = consented_gate();
        // Local when the refresh starts; Cloud lands while it verifies.
        let engine = gate.engine.clone();
        gate.test_before_refresh = Some(Box::new(move || {
            record_engine(&engine, AsrMode::Cloud, 1);
        }));
        assert_eq!(gate.refresh_passive(), LocalReadiness::Absent);
        assert_eq!(gate.launch_count, 0);
        assert!(!gate.lifecycle.has_child());
        assert_eq!(published_snapshot(&gate.published), LocalReadiness::Absent);
    }

    #[test]
    fn committed_cloud_switch_unloads_the_global_worker() {
        let _session = test_session();
        inject_ready(fixture_receipt(), "hello");
        assert!(local_worker_loaded_for_tests());
        apply_engine_selection(AsrMode::Cloud, TEST_ENGINE_REVISION + 1);
        join_cloud_unloads_for_tests();
        assert!(!local_worker_loaded_for_tests());
        assert_eq!(readiness_snapshot(), LocalReadiness::Absent);
    }

    #[test]
    fn a_stale_cloud_unload_after_a_newer_local_switch_keeps_the_worker() {
        let _session = test_session();
        inject_ready(fixture_receipt(), "hello");
        {
            // The Cloud(N) unload thread waits behind the gate while Local(N+1)
            // commits; it must see the newer choice when it finally runs.
            let _held = gate().lock().unwrap();
            apply_engine_selection(AsrMode::Cloud, TEST_ENGINE_REVISION + 1);
            apply_engine_selection(AsrMode::Local, TEST_ENGINE_REVISION + 2);
        }
        join_cloud_unloads_for_tests();
        assert!(local_worker_loaded_for_tests());
        assert!(matches!(admit_local(true), LocalAdmission::Ready { .. }));
    }

    #[test]
    fn work_admitted_before_a_cloud_switch_unloads_on_release_off_the_caller() {
        let _session = test_session();
        inject_ready(fixture_receipt(), "hello");
        assert!(matches!(admit_local(true), LocalAdmission::Ready { .. }));
        let unstarted = UnstartedAdmission::claim(true);
        apply_engine_selection(AsrMode::Cloud, TEST_ENGINE_REVISION + 1);
        join_cloud_unloads_for_tests();
        assert!(local_worker_loaded_for_tests(), "admitted work keeps it");
        {
            // The release runs on this thread with the gate held throughout:
            // the reap must wait for the unload thread, not happen inline.
            let mut held = gate().lock().unwrap();
            held.release_admission();
            assert!(held.lifecycle.has_child());
        }
        unstarted.hand_off();
        join_cloud_unloads_for_tests();
        assert!(!local_worker_loaded_for_tests());
        assert_eq!(readiness_snapshot(), LocalReadiness::Absent);
    }

    #[test]
    fn switch_to_cloud_while_busy_finishes_the_work_then_unloads() {
        let (_state, mut gate, receipt) = consented_gate();
        gate.test_next_worker = Some(next_consented_worker(&receipt));
        assert!(matches!(gate.refresh(), LocalReadiness::Ready { .. }));
        assert!(matches!(gate.admit(true), LocalAdmission::Ready { .. }));
        // Admitted, capture not finished: the switch keeps the worker.
        gate.select_engine(AsrMode::Cloud, 1);
        assert!(gate.lifecycle.has_child());
        assert_eq!(gate.refresh_passive(), LocalReadiness::Busy);
        assert!(gate.lifecycle.has_child());
        // Inference checks the supervisor out exactly as take_inference_session.
        let mut supervisor = gate.lifecycle.take_supervisor();
        gate.inference_checked_out = true;
        gate.select_engine(AsrMode::Cloud, 1);
        assert_eq!(gate.refresh_passive(), LocalReadiness::Busy);
        let outcome = transcribe_through_supervisor(
            &mut supervisor,
            correlation(&receipt.receipt_hash, "rec-busy", "rec-busy-asr"),
            vec![1, 0, 3, 0],
            None,
            &gate.sentinel,
        )
        .expect("in-flight work completes");
        assert!(matches!(
            outcome,
            WorkerOutcome::Transcript { ref text, .. } if text == "still delivered"
        ));
        // restore_inference_session: the worker returns, then unloads once idle.
        gate.lifecycle.restore_supervisor(supervisor);
        gate.inference_checked_out = false;
        assert!(gate.lifecycle.has_child());
        // Release runs on the daemon actor: it schedules the drain instead of
        // reaping inline.
        let scheduled = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let count = scheduled.clone();
        gate.schedule_unload = Box::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
            true
        });
        gate.release_admission();
        assert_eq!(scheduled.load(Ordering::SeqCst), 1);
        assert!(gate.lifecycle.has_child(), "no inline reap on release");
        // The unload thread, under the gate lock.
        gate.unload_for_cloud();
        assert!(!gate.lifecycle.has_child());
        assert_eq!(gate.lifecycle.state(), WorkerState::Absent);
        assert_eq!(published_snapshot(&gate.published), LocalReadiness::Absent);
        assert_eq!(gate.launch_count, 1);
    }

    #[test]
    fn a_local_model_tier_reads_as_its_friendly_name_and_device() {
        use crate::config::LocalDevice;
        let receipt = |id: &str| ActiveReceipt {
            catalog_id: id.into(),
            catalog_revision: "whisper.cpp-1.9.5".into(),
            runtime_abi: "voisu-consented:whisper.cpp-1.9.5@gpu".into(),
            protocol: crate::local_worker::PROTOCOL_VERSION,
            language: "en".into(),
            artifact_id: "a".repeat(64),
            files: std::collections::BTreeMap::new(),
            receipt_hash: "b".repeat(64),
        };
        let accurate = receipt("whisper-cpp-distil-large-v3.5-q8-0");
        let fast = receipt("whisper-cpp-small-en-q8-0");
        assert_eq!(
            model_identity_on(&accurate, || LocalDevice::Auto),
            "Accurate (distil-large-v3.5, GPU)"
        );
        assert_eq!(
            model_identity_on(&fast, || LocalDevice::Cpu),
            "Fast (small.en, CPU)"
        );
        let status = voisu_core::AsrModeStatus {
            pending: Some(AsrMode::Local),
            local_readiness: LocalReadiness::Ready {
                model_identity: Some(model_identity_on(&fast, || LocalDevice::Cpu)),
            },
            active: None,
            revision: None,
            admission_error: None,
            audio_retention: None,
            local_path_clean: Some(true),
        };
        let text = crate::local_status::format_cli_status("status", Some(&status));
        assert!(
            text.contains("Local model: ready (Fast (small.en, CPU))\n"),
            "{text}"
        );
        assert!(!text.contains("whisper-cpp-small-en-q8-0"), "{text}");
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
            LITERAL,
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
        let completion =
            complete_recording("rec-1", vec![1, 0, 2, 0], &[], LITERAL, false, false).unwrap();
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

    /// Runs one Recording through an injected worker and reports what the
    /// worker saw: `(transcribe_count, last_prompt)`. The fake starts with a
    /// sentinel prompt, so a Transcribe that never arrived cannot read as "no
    /// prompt".
    fn worker_saw(accepts_prompt: bool, terms: &[&str]) -> (u32, Option<String>) {
        worker_saw_for(fixture_receipt(), accepts_prompt, terms)
    }

    fn worker_saw_for(
        receipt: ActiveReceipt,
        accepts_prompt: bool,
        terms: &[&str],
    ) -> (u32, Option<String>) {
        inject_ready_with_prompt_support(receipt, "hello", accepts_prompt);
        gate()
            .lock()
            .expect("gate")
            .lifecycle
            .child_mut()
            .unwrap()
            .last_prompt = Some("sentinel".into());
        begin_live("rec-prompt");
        let terms: Vec<String> = terms.iter().map(|term| (*term).to_owned()).collect();
        complete_recording(
            "rec-prompt",
            vec![1, 0, 2, 0],
            &terms,
            LITERAL,
            false,
            false,
        )
        .unwrap();
        let mut gate = gate().lock().expect("gate");
        let child = gate.lifecycle.child_mut().unwrap();
        (child.transcribe_count, child.last_prompt.clone())
    }

    #[test]
    fn a_prompt_capable_worker_gets_the_dictionary_as_its_initial_prompt() {
        let _lock = test_session();
        let terms = ["Voisu".to_owned(), "Tokio".to_owned()];
        let (count, prompt) = worker_saw(true, &["Voisu", "Tokio"]);
        assert_eq!(count, 1);
        let prompt = prompt.expect("a prompt");
        assert!(prompt.starts_with("Glossary: Voisu, Tokio, "), "{prompt:?}");
        assert_eq!(
            Some(prompt),
            crate::dictionary::local_prompt_for_terms(&terms, true)
        );
    }

    #[test]
    fn a_prompt_capable_worker_with_an_empty_dictionary_gets_the_built_in_glossary() {
        let _lock = test_session();
        assert_eq!(
            worker_saw(true, &[]),
            (1, crate::dictionary::local_prompt_for_terms(&[], true))
        );
    }

    #[test]
    fn a_non_english_model_gets_only_the_bare_user_terms_as_its_prompt() {
        let _lock = test_session();
        let mut receipt = fixture_receipt();
        receipt.language = "auto".to_owned();
        assert_eq!(
            worker_saw_for(receipt.clone(), true, &["Voisu", "Tokio"]),
            (1, Some("Voisu, Tokio".to_owned()))
        );
        assert_eq!(worker_saw_for(receipt, true, &[]), (1, None));
    }

    #[test]
    fn a_worker_that_did_not_advertise_prompt_support_never_gets_one() {
        let _lock = test_session();
        assert_eq!(worker_saw(false, &["Voisu", "Tokio"]), (1, None));
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
        let completion =
            complete_recording("rec-silent", vec![0, 0, 0, 0], &[], LITERAL, false, false).unwrap();
        // Digital silence skips the speech check and keeps its own reason.
        assert_eq!(
            completion.decision,
            TerminalDecision::NoText {
                reason: "silence".into()
            }
        );
        assert_eq!(completion.local_asr.speech_peak_permille, None);
        let err = complete_replay("rec-old", vec![1, 0], &[], LITERAL, true, true).unwrap_err();
        assert!(matches!(err, LocalError::CloudSelected));
    }

    fn pcm_bytes(samples: impl IntoIterator<Item = i16>) -> Vec<u8> {
        samples.into_iter().flat_map(i16::to_le_bytes).collect()
    }

    /// Two seconds of loud mains hum: the capture energy gate passes it.
    fn mains_hum() -> Vec<u8> {
        pcm_bytes((0..32_000).map(|n| {
            (6_000.0 * (2.0 * std::f64::consts::PI * 50.0 * f64::from(n) / 16_000.0).sin()) as i16
        }))
    }

    #[test]
    fn steady_noise_is_refused_before_the_local_model_transcribes() {
        let _lock = test_session();
        // The scripted worker answers every request with this text, as
        // Whisper answers room noise with "Okay.", which no phrase filter
        // can drop without dropping a real "Okay.".
        inject_ready(fixture_receipt(), "Okay.");
        begin_live("rec-hum");
        let completion =
            complete_recording("rec-hum", mains_hum(), &[], LITERAL, false, false).unwrap();
        assert_eq!(
            completion.decision,
            TerminalDecision::NoText {
                reason: "no speech".into()
            }
        );
        assert_eq!(completion.local_asr.reason.as_deref(), Some("no speech"));
        let peak = completion.local_asr.speech_peak_permille.unwrap();
        assert!(peak < 200, "refused at peak {peak}");
        assert!(completion.local_asr.speech_gain_percent.is_some());
    }

    #[test]
    fn replay_of_steady_noise_still_reaches_the_local_model() {
        let _lock = test_session();
        inject_ready(fixture_receipt(), "Okay.");
        // Replay is an explicit request to transcribe again, so the speech
        // check must not stand between it and the model.
        let decision =
            complete_replay("rec-hum-replay", mains_hum(), &[], LITERAL, false, false).unwrap();
        assert_eq!(decision, TerminalDecision::Transcript("Okay.".into()));
    }

    #[test]
    fn speech_still_reaches_the_local_model() {
        let _lock = test_session();
        inject_ready(fixture_receipt(), "hello there");
        begin_live("rec-speech");
        let wav = include_bytes!("../tests/fixtures/jfk-3s.wav");
        let completion =
            complete_recording("rec-speech", wav[44..].to_vec(), &[], LITERAL, false, false)
                .unwrap();
        assert_eq!(
            completion.decision,
            TerminalDecision::Transcript("hello there".into())
        );
        // A pass is recorded too: the first window at or above the threshold.
        let peak = completion.local_asr.speech_peak_permille.unwrap();
        assert!(peak >= 200, "passed at peak {peak}");
        assert_eq!(completion.local_asr.speech_gain_percent, Some(100));
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
            transcribe_through_supervisor(&mut supervisor, corr, vec![1, 0], None, &sentinel)
                .is_err()
        );
    }

    const IDLE: Option<std::time::Duration> = Some(std::time::Duration::from_secs(300));

    fn secs(n: u64) -> std::time::Duration {
        std::time::Duration::from_secs(n)
    }

    /// A consented gate with a Ready worker loaded at `base` on the fake clock.
    fn idle_consented_gate() -> (tempfile::TempDir, Gate, ActiveReceipt, Instant) {
        let (state, mut gate, receipt) = consented_gate();
        let base = Instant::now();
        gate.test_clock = Some(base);
        assert!(matches!(gate.refresh(), LocalReadiness::Ready { .. }));
        assert!(gate.lifecycle.has_child());
        (state, gate, receipt, base)
    }

    #[test]
    fn an_idle_worker_unloads_after_the_timeout_and_stays_ready() {
        let (_state, mut gate, _, base) = idle_consented_gate();
        assert_eq!(gate.unload_if_idle(base + secs(299), IDLE), Some(secs(1)));
        assert!(gate.lifecycle.has_child());
        assert_eq!(gate.unload_if_idle(base + secs(300), IDLE), None);
        // The child was cancelled and reaped: nothing holds RAM/VRAM.
        assert!(!gate.lifecycle.has_child());
        assert_eq!(gate.lifecycle.state(), WorkerState::Absent);
        // Status still says ready: the next Recording reloads it.
        let published = published_snapshot(&gate.published);
        assert!(
            matches!(published, LocalReadiness::Ready { .. }),
            "{published:?}"
        );
        assert!(matches!(gate.refresh(), LocalReadiness::Ready { .. }));
        assert!(matches!(
            gate.refresh_passive(),
            LocalReadiness::Ready { .. }
        ));
        assert_eq!(gate.launch_count, 1, "status refresh does not reload");
    }

    #[test]
    fn activity_before_the_timeout_restarts_the_timer() {
        let (_state, mut gate, _, base) = idle_consented_gate();
        gate.test_clock = Some(base + secs(200));
        assert!(matches!(gate.admit(true), LocalAdmission::Ready { .. }));
        gate.release_admission();
        assert_eq!(gate.unload_if_idle(base + secs(350), IDLE), Some(secs(150)));
        assert!(gate.lifecycle.has_child());
        assert_eq!(gate.unload_if_idle(base + secs(500), IDLE), None);
        assert!(!gate.lifecycle.has_child());
    }

    #[test]
    fn busy_work_at_the_timeout_is_never_unloaded() {
        let (_state, mut gate, _, base) = idle_consented_gate();
        assert!(matches!(gate.admit(true), LocalAdmission::Ready { .. }));
        assert_eq!(gate.unload_if_idle(base + secs(10_000), IDLE), None);
        assert!(gate.lifecycle.has_child());
        // Inference has the supervisor checked out, as take_inference_session.
        let supervisor = gate.lifecycle.take_supervisor();
        gate.inference_checked_out = true;
        assert_eq!(gate.unload_if_idle(base + secs(10_000), IDLE), None);
        gate.lifecycle.restore_supervisor(supervisor);
        gate.inference_checked_out = false;
        assert!(gate.lifecycle.has_child());
    }

    #[test]
    fn idle_unload_off_never_unloads() {
        let (_state, mut gate, _, base) = idle_consented_gate();
        assert_eq!(gate.unload_if_idle(base + secs(86_400 * 30), None), None);
        assert!(gate.lifecycle.has_child());
    }

    #[test]
    fn recording_start_reloads_an_idle_unloaded_worker_while_capture_runs() {
        let (_state, mut gate, receipt, base) = idle_consented_gate();
        gate.unload_if_idle(base + secs(300), IDLE);
        gate.test_next_worker = Some(next_consented_worker(&receipt));
        // Start admits at once; the load runs off the gate while the user speaks.
        assert!(matches!(gate.admit(true), LocalAdmission::Ready { .. }));
        // Verify and launch belong to the load, not to Start.
        assert_eq!(gate.launch_count, 1);
        let load = gate.pending_load.take().expect("load started at Start");
        assert!(gate.loading_in_background);
        assert_eq!(gate.refresh(), LocalReadiness::Busy);
        assert_eq!(gate.launch_count, 1, "refresh never launches beside it");
        gate.run_load_here(load);
        assert_eq!(gate.launch_count, 2);
        assert!(!gate.loading_in_background);
        assert_eq!(gate.lifecycle.state(), WorkerState::Ready);
        assert!(gate.lifecycle.has_child());
        assert_eq!(
            gate.admitted_generation,
            Some(gate.lifecycle.generation()),
            "the reservation pins the reloaded worker"
        );
        gate.release_admission();
        assert!(matches!(
            published_snapshot(&gate.published),
            LocalReadiness::Ready { .. }
        ));
    }

    #[test]
    fn recording_start_with_cloud_selected_launches_nothing() {
        let (_state, mut gate, receipt, base) = idle_consented_gate();
        gate.unload_if_idle(base + secs(300), IDLE);
        gate.test_next_worker = Some(next_consented_worker(&receipt));
        record_engine(&gate.engine, AsrMode::Cloud, 1);
        assert!(matches!(
            gate.admit(true),
            LocalAdmission::Unavailable { .. }
        ));
        assert!(gate.pending_load.is_none());
        assert_eq!(gate.launch_count, 1);
        assert!(!gate.lifecycle.has_child());
        // Q0: Cloud shows an unloaded worker as Absent, not ready.
        gate.unload_for_cloud();
        assert_eq!(published_snapshot(&gate.published), LocalReadiness::Absent);
    }

    /// The global gate after an idle unload, with the reload worker queued.
    fn idle_unloaded_global(next: FakeWorker) -> Instant {
        inject_ready(fixture_receipt(), "first");
        let mut held = gate().lock().unwrap();
        let base = Instant::now();
        held.test_clock = Some(base);
        held.idle_since = Some(base);
        held.unload_if_idle(base + secs(300), IDLE);
        assert!(!held.lifecycle.has_child());
        held.test_next_worker = Some(next);
        base
    }

    #[test]
    fn stop_waits_for_a_load_still_running_then_transcribes() {
        let _session = test_session();
        idle_unloaded_global(FakeWorker {
            scripted_text: Some("reloaded".into()),
            ..FakeWorker::default()
        });
        let (release, hold) = std::sync::mpsc::channel::<()>();
        gate().lock().unwrap().test_load_hook = Some(Box::new(move |supervisor| {
            hold.recv().unwrap();
            // infer_unlocked's fixed test correlation names generation 1.
            if let Some(child) = supervisor.child_mut() {
                child.generation = 1;
            }
        }));
        let waits_before = LOAD_WAITS.load(Ordering::SeqCst);
        assert!(matches!(admit_local(true), LocalAdmission::Ready { .. }));
        begin_live("rec-reload");
        let stop = std::thread::spawn(|| {
            complete_recording("rec-reload", vec![1, 0, 2, 0], &[], LITERAL, false, false)
        });
        // Stop is parked on the load, not failed: wait until it waits.
        while LOAD_WAITS.load(Ordering::SeqCst) == waits_before {
            std::thread::yield_now();
        }
        assert!(!stop.is_finished());
        release.send(()).unwrap();
        let completion = stop.join().unwrap().expect("transcribed after the load");
        assert_eq!(
            completion.decision,
            TerminalDecision::Transcript("reloaded".to_owned())
        );
        join_background_loads_for_tests();
        assert!(local_worker_loaded_for_tests());
    }

    #[test]
    fn a_load_past_the_bound_fails_closed_at_stop() {
        let _session = test_session();
        idle_unloaded_global(FakeWorker {
            scripted_text: Some("never".into()),
            block_for: Some(crate::local_worker::LOAD_DEADLINE + secs(1)),
            ..FakeWorker::default()
        });
        assert!(matches!(admit_local(true), LocalAdmission::Ready { .. }));
        begin_live("rec-slow");
        let result = complete_recording("rec-slow", vec![1, 0, 2, 0], &[], LITERAL, false, false);
        assert!(matches!(result, Err(LocalError::Unavailable)), "{result:?}");
        join_background_loads_for_tests();
        let published = readiness_snapshot();
        assert!(
            matches!(published, LocalReadiness::Unavailable { .. }),
            "{published:?}"
        );
    }

    impl Gate {
        /// The load thread's steps, on a gate the test owns.
        fn run_load_here(&mut self, load: PendingLoad) {
            let PendingLoad { mut launch, hook } = load;
            let launched = launch.run();
            let Some(mut preparing) = self.attach_launched(launch, launched) else {
                return;
            };
            let result = preparing
                .supervisor
                .prepare(preparing.correlation, Instant::now())
                .map(|_| ())
                .map_err(|error| error.to_string());
            if let Some(hook) = hook {
                hook(&mut preparing.supervisor);
            }
            self.finish_load(FinishedLoad {
                supervisor: preparing.supervisor,
                selected: preparing.selected,
                result,
            });
        }
    }

    /// Blocks the reload's `Prepare` until the returned sender fires.
    fn hold_the_load() -> std::sync::mpsc::Sender<()> {
        let (release, hold) = std::sync::mpsc::channel::<()>();
        gate().lock().unwrap().test_load_hook = Some(Box::new(move |supervisor| {
            let _ = hold.recv();
            // infer_unlocked's fixed test correlation names generation 1.
            if let Some(child) = supervisor.child_mut() {
                child.generation = 1;
            }
        }));
        release
    }

    fn reloaded_worker() -> FakeWorker {
        FakeWorker {
            scripted_text: Some("reloaded".into()),
            ..FakeWorker::default()
        }
    }

    #[test]
    fn recording_start_never_verifies_or_launches_on_the_caller() {
        let _session = test_session();
        idle_unloaded_global(reloaded_worker());
        let (release, hold) = std::sync::mpsc::channel::<()>();
        gate().lock().unwrap().test_before_launch = Some(Box::new(move || {
            let _ = hold.recv();
        }));
        let (sent, admitted) = std::sync::mpsc::channel();
        std::thread::spawn(move || sent.send(admit_local(true)).unwrap());
        // Verify + launch are parked on the barrier: Start must not wait on it.
        let admission = admitted.recv_timeout(secs(10));
        release.send(()).unwrap();
        assert!(
            matches!(admission, Ok(LocalAdmission::Ready { .. })),
            "{admission:?}"
        );
        abandon_live("never-begun");
        join_background_loads_for_tests();
        assert!(local_worker_loaded_for_tests());
        assert_eq!(gate().lock().unwrap().launch_count, 1);
    }

    #[test]
    fn a_failed_launch_after_start_fails_closed_at_stop() {
        let _session = test_session();
        idle_unloaded_global(reloaded_worker());
        gate().lock().unwrap().test_next_worker = None;
        assert!(matches!(admit_local(true), LocalAdmission::Ready { .. }));
        begin_live("rec-no-launch");
        let result = complete_recording(
            "rec-no-launch",
            vec![1, 0, 2, 0],
            &[],
            LITERAL,
            false,
            false,
        );
        assert!(matches!(result, Err(LocalError::Unavailable)), "{result:?}");
        join_background_loads_for_tests();
        assert!(matches!(
            readiness_snapshot(),
            LocalReadiness::Unavailable { .. }
        ));
    }

    #[test]
    fn a_start_abandoned_during_a_reload_lets_the_next_start_join_it() {
        let _session = test_session();
        idle_unloaded_global(reloaded_worker());
        let release = hold_the_load();
        assert!(matches!(admit_local(true), LocalAdmission::Ready { .. }));
        begin_live("rec-dropped");
        abandon_live("rec-dropped");
        // Production Start checks readiness through `refresh` before it
        // admits, so the join must be visible there too.
        let precheck = gate().lock().unwrap().refresh();
        assert!(precheck.admits_capture(), "{precheck:?}");
        let again = admit_local(true);
        assert!(matches!(again, LocalAdmission::Ready { .. }), "{again:?}");
        begin_live("rec-joined");
        let waits_before = LOAD_WAITS.load(Ordering::SeqCst);
        let stop = std::thread::spawn(|| {
            complete_recording("rec-joined", vec![1, 0, 2, 0], &[], LITERAL, false, false)
        });
        while LOAD_WAITS.load(Ordering::SeqCst) == waits_before {
            std::thread::yield_now();
        }
        release.send(()).unwrap();
        let completion = stop.join().unwrap().expect("transcribed after the load");
        assert_eq!(
            completion.decision,
            TerminalDecision::Transcript("reloaded".to_owned())
        );
        join_background_loads_for_tests();
    }

    #[test]
    fn stop_gives_up_on_a_load_past_its_wait() {
        let _session = test_session();
        idle_unloaded_global(reloaded_worker());
        let release = hold_the_load();
        gate().lock().unwrap().test_load_wait = Some(Duration::from_millis(50));
        assert!(matches!(admit_local(true), LocalAdmission::Ready { .. }));
        begin_live("rec-overdue");
        let waits_before = LOAD_WAITS.load(Ordering::SeqCst);
        let result =
            complete_recording("rec-overdue", vec![1, 0, 2, 0], &[], LITERAL, false, false);
        assert!(matches!(result, Err(LocalError::Unavailable)), "{result:?}");
        assert!(LOAD_WAITS.load(Ordering::SeqCst) > waits_before);
        release.send(()).unwrap();
        join_background_loads_for_tests();
        // The late load still lands, ready for the next Recording.
        assert!(matches!(readiness_snapshot(), LocalReadiness::Ready { .. }));
    }

    #[test]
    fn a_failed_reload_at_stop_keeps_the_recovery_audio() {
        let _session = test_session();
        let recovery = tempfile::tempdir().unwrap();
        *TEST_RECOVERY_DIR.lock().unwrap() = Some(recovery.path().join("local-recovery"));
        idle_unloaded_global(FakeWorker {
            block_for: Some(crate::local_worker::LOAD_DEADLINE + secs(1)),
            ..FakeWorker::default()
        });
        assert!(matches!(admit_local(true), LocalAdmission::Ready { .. }));
        begin_live("rec-kept");
        let pcm = vec![1, 0, 2, 0];
        let result = complete_recording("rec-kept", pcm.clone(), &[], LITERAL, true, false);
        assert!(matches!(result, Err(LocalError::Unavailable)), "{result:?}");
        join_background_loads_for_tests();
        let store = RecoveryStore::open(
            recovery.path().join("local-recovery"),
            RecoveryClock::system(),
        )
        .unwrap();
        assert_eq!(store.read_pcm("rec-kept").unwrap(), pcm);
    }

    #[test]
    fn a_panicking_load_keeps_its_supervisor() {
        let _session = test_session();
        idle_unloaded_global(reloaded_worker());
        gate().lock().unwrap().test_load_hook =
            Some(Box::new(|_| panic!("load thread panicked on purpose")));
        assert!(matches!(admit_local(true), LocalAdmission::Ready { .. }));
        abandon_live("never-begun");
        join_background_loads_for_tests();
        // The worker (and the restart budget it carries) came back to the
        // gate; only readiness failed.
        assert!(local_worker_loaded_for_tests());
        assert!(matches!(
            readiness_snapshot(),
            LocalReadiness::Unavailable { .. }
        ));
    }
}
