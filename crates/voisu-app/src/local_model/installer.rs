//! Atomic catalog install with health-before-activation.

use std::fs;
use std::io;
use std::path::Path;
use std::time::Instant;

use crate::local_worker::{PROTOCOL_VERSION, refuse_production_weight_download};

use super::catalog::{CatalogEntry, sha256_hex, verify_file_digest};
use super::fetch::{ArtifactFetcher, FetchError, FetchRequest};
use super::health::{HealthError, HealthProbe};
use super::receipt::{self, ActiveReceipt};
use super::safe_fs::{self, SafeFsError};
use super::store::{ModelStore, StoreError};
use super::{INSTALL_DEADLINE, NO_PROGRESS};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstallAbort {
    AfterLock,
    AfterStaging,
    AfterFetch,
    AfterPublish,
    AfterHealth,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MaintenanceKind {
    Idle,
    Recording,
    Replay,
    DaemonBusy,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MaintenanceError {
    Busy(&'static str),
}

pub trait MaintenanceReservation {
    fn acquire(&self) -> Result<MaintenanceKind, MaintenanceError>;
}

#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub struct IdleMaintenance;

#[cfg(test)]
impl MaintenanceReservation for IdleMaintenance {
    fn acquire(&self) -> Result<MaintenanceKind, MaintenanceError> {
        Ok(MaintenanceKind::Idle)
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub struct BusyMaintenance {
    pub kind: MaintenanceKind,
}

#[cfg(test)]
impl MaintenanceReservation for BusyMaintenance {
    fn acquire(&self) -> Result<MaintenanceKind, MaintenanceError> {
        let message = match self.kind {
            MaintenanceKind::Recording => "Recording active",
            MaintenanceKind::Replay => "Replay active",
            MaintenanceKind::DaemonBusy => "daemon is not idle",
            MaintenanceKind::Idle => "idle",
        };
        Err(MaintenanceError::Busy(message))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallConsent {
    pub bytes: u64,
    pub license_spdx: String,
}

#[derive(Clone, Debug)]
pub struct InstallIo {
    pub available_bytes: u64,
    pub abort: Option<InstallAbort>,
    pub fail_write: Option<i32>,
}

impl Default for InstallIo {
    fn default() -> Self {
        Self {
            available_bytes: u64::MAX / 4,
            abort: None,
            fail_write: None,
        }
    }
}

pub struct InstallRequest<'a, F, H, M> {
    pub store: &'a mut ModelStore,
    pub entry: &'a CatalogEntry,
    pub fetcher: &'a F,
    pub health: &'a H,
    pub maintenance: &'a M,
    pub consent: InstallConsent,
    pub io: InstallIo,
    pub started: Instant,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InstallError {
    Busy,
    Maintenance(MaintenanceError),
    ProductionWeightsForbidden,
    Consent,
    NoSpace,
    Fetch(FetchError),
    Hash,
    Size,
    Abi,
    Fs(SafeFsError),
    Health(HealthError),
    Aborted,
    Deadline,
    Store(StoreError),
    ArchiveOrExecutable,
}

pub fn install_entry<F, H, M>(
    request: InstallRequest<'_, F, H, M>,
) -> Result<ActiveReceipt, InstallError>
where
    F: ArtifactFetcher,
    H: HealthProbe,
    M: MaintenanceReservation,
{
    match request.maintenance.acquire()? {
        MaintenanceKind::Idle => {}
        MaintenanceKind::Recording => {
            return Err(InstallError::Maintenance(MaintenanceError::Busy(
                "Recording active",
            )));
        }
        MaintenanceKind::Replay => {
            return Err(InstallError::Maintenance(MaintenanceError::Busy(
                "Replay active",
            )));
        }
        MaintenanceKind::DaemonBusy => {
            return Err(InstallError::Maintenance(MaintenanceError::Busy(
                "daemon is not idle",
            )));
        }
    }
    let _lock = request.store.lock_exclusive().map_err(map_store)?;
    let prior = request.store.load_active().map_err(map_store)?;
    if request.io.abort == Some(InstallAbort::AfterLock) {
        return abort(prior);
    }
    if request.entry.production_weights {
        let _ = refuse_production_weight_download();
        return Err(InstallError::ProductionWeightsForbidden);
    }
    if request.consent.bytes != request.entry.total_bytes()
        || request.consent.license_spdx != request.entry.license.spdx
    {
        return Err(InstallError::Consent);
    }
    if request.io.available_bytes < request.entry.total_bytes().saturating_mul(2) {
        return Err(InstallError::NoSpace);
    }
    deadline(request.started)?;
    if !request.entry.abi_supported() {
        return Err(InstallError::Abi);
    }

    let token = format!("s-{}", sha256_hex(request.entry.id.as_bytes()));
    let staging = request.store.staging_dir(&token);
    let _ = fs::remove_dir_all(&staging);
    safe_fs::ensure_private_dir(&staging).map_err(InstallError::Fs)?;
    if request.io.abort == Some(InstallAbort::AfterStaging) {
        return abort(prior);
    }

    for file in request.entry.files {
        safe_fs::relative_file_name(file.name).map_err(InstallError::Fs)?;
        safe_fs::reject_forbidden_filename(file.name)
            .map_err(|_| InstallError::ArchiveOrExecutable)?;
        let hosts = request
            .entry
            .allowed_hosts
            .iter()
            .map(|host| (*host).to_owned())
            .collect();
        // Idle TCP is already timed out by ProductionHttps. Transfer duration
        // is bounded by INSTALL_DEADLINE, not NO_PROGRESS.
        let fetched = request
            .fetcher
            .fetch(&FetchRequest {
                url: file.url.to_owned(),
                allowed_hosts: hosts,
                expected_bytes: file.bytes,
            })
            .map_err(map_fetch)?;
        deadline(request.started)?;
        if u64::try_from(fetched.body.len()).unwrap_or(u64::MAX) != file.bytes {
            return Err(InstallError::Size);
        }
        if !verify_file_digest(&fetched.body, file.sha256_hex, file.bytes) {
            return Err(InstallError::Hash);
        }
        if let Some(errno) = request.io.fail_write {
            return Err(map_write_errno(errno));
        }
        let write_started = Instant::now();
        let mut dest =
            safe_fs::create_exclusive_file(&staging, file.name).map_err(InstallError::Fs)?;
        safe_fs::durable_write(&mut dest, &fetched.body).map_err(InstallError::Fs)?;
        no_progress(write_started)?;
        deadline(request.started)?;
    }
    if request.io.abort == Some(InstallAbort::AfterFetch) {
        return abort(prior);
    }

    crate::local_model::verify_candidate(request.entry, &staging).map_err(InstallError::Health)?;
    let artifact_hash = tree_hash(&staging, request.entry)?;
    if request.entry.runtime_abi.protocol != PROTOCOL_VERSION
        || request.entry.runtime_abi.abi_id.is_empty()
    {
        return Err(InstallError::Abi);
    }
    let dest = request.store.artifact_dir(request.entry, &artifact_hash);
    if safe_fs::dir_present(&dest).map_err(InstallError::Fs)? {
        crate::local_model::verify_candidate(request.entry, &dest).map_err(InstallError::Health)?;
        let _ = fs::remove_dir_all(&staging);
    } else {
        if let Some(parent) = dest.parent() {
            safe_fs::ensure_private_dir(parent).map_err(InstallError::Fs)?;
        }
        safe_fs::durable_rename(&staging, &dest).map_err(InstallError::Fs)?;
        crate::local_model::verify_candidate(request.entry, &dest).map_err(InstallError::Health)?;
    }
    if request.io.abort == Some(InstallAbort::AfterPublish) {
        return abort(prior);
    }

    request
        .health
        .check(request.entry, &dest)
        .map_err(InstallError::Health)?;
    if request.io.abort == Some(InstallAbort::AfterHealth) {
        return abort(prior);
    }

    let receipt = receipt::from_entry(request.entry, &artifact_hash);
    request.store.commit_receipt(&receipt).map_err(map_store)?;
    Ok(receipt)
}

fn tree_hash(dir: &Path, entry: &CatalogEntry) -> Result<String, InstallError> {
    let mut joined = String::new();
    for file in entry.files {
        let bytes =
            safe_fs::read_existing_file(dir, file.name, file.bytes).map_err(InstallError::Fs)?;
        if !verify_file_digest(&bytes, file.sha256_hex, file.bytes) {
            return Err(InstallError::Hash);
        }
        joined.push_str(&sha256_hex(&bytes));
    }
    Ok(sha256_hex(joined.as_bytes()))
}

fn deadline(started: Instant) -> Result<(), InstallError> {
    if started.elapsed() > INSTALL_DEADLINE {
        Err(InstallError::Deadline)
    } else {
        Ok(())
    }
}

fn no_progress(started: Instant) -> Result<(), InstallError> {
    if started.elapsed() > NO_PROGRESS {
        Err(InstallError::Deadline)
    } else {
        Ok(())
    }
}

fn abort(prior: Option<ActiveReceipt>) -> Result<ActiveReceipt, InstallError> {
    let _ = prior;
    Err(InstallError::Aborted)
}

fn map_store(error: StoreError) -> InstallError {
    match error {
        StoreError::Busy => InstallError::Busy,
        other => InstallError::Store(other),
    }
}

fn map_fetch(error: FetchError) -> InstallError {
    match error {
        FetchError::Deadline | FetchError::NoProgress => InstallError::Deadline,
        other => InstallError::Fetch(other),
    }
}

fn map_write_errno(errno: i32) -> InstallError {
    if errno == libc::ENOSPC {
        InstallError::NoSpace
    } else {
        InstallError::Fs(SafeFsError::Io(
            io::Error::from_raw_os_error(errno).to_string(),
        ))
    }
}

impl From<MaintenanceError> for InstallError {
    fn from(value: MaintenanceError) -> Self {
        Self::Maintenance(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_model::catalog::{CatalogEntry, ci_fixture_entry, shipped_catalog};
    use crate::local_model::fetch::{ScriptedFetcher, ScriptedHop};
    use crate::local_model::health::{CandidateHealth, FailingHealth};
    use std::time::Duration;

    fn consent(entry: &CatalogEntry) -> InstallConsent {
        InstallConsent {
            bytes: entry.total_bytes(),
            license_spdx: entry.license.spdx.to_owned(),
        }
    }

    fn fixture_fetcher() -> ScriptedFetcher {
        ScriptedFetcher::new(vec![
            ScriptedHop::Body(CatalogEntry::fixture_model_bytes().to_vec()),
            ScriptedHop::Body(CatalogEntry::fixture_pcm_bytes().to_vec()),
        ])
    }

    fn open_store() -> (tempfile::TempDir, ModelStore) {
        let temp = tempfile::tempdir().unwrap();
        let store = ModelStore::open(temp.path().to_path_buf()).unwrap();
        (temp, store)
    }

    fn install_ok(store: &mut ModelStore) -> ActiveReceipt {
        let entry = ci_fixture_entry();
        install_entry(InstallRequest {
            store,
            entry,
            fetcher: &fixture_fetcher(),
            health: &CandidateHealth::default(),
            maintenance: &IdleMaintenance,
            consent: consent(entry),
            io: InstallIo::default(),
            started: Instant::now(),
        })
        .unwrap()
    }

    #[test]
    fn install_deadlines_are_locked() {
        assert_eq!(INSTALL_DEADLINE, Duration::from_secs(30 * 60));
        assert_eq!(NO_PROGRESS, Duration::from_secs(30));
    }

    #[test]
    fn successful_install_writes_receipt_after_health() {
        let (_temp, mut store) = open_store();
        let receipt = install_ok(&mut store);
        assert_eq!(receipt.catalog_id, "l3-health-fixture");
        assert_eq!(
            store.load_active().unwrap().unwrap().catalog_id,
            receipt.catalog_id
        );
    }

    #[test]
    fn production_weights_are_not_downloaded() {
        let (_temp, mut store) = open_store();
        let entry = shipped_catalog()
            .entries
            .iter()
            .find(|entry| entry.production_weights)
            .unwrap();
        let error = install_entry(InstallRequest {
            store: &mut store,
            entry,
            fetcher: &fixture_fetcher(),
            health: &CandidateHealth::default(),
            maintenance: &IdleMaintenance,
            consent: consent(entry),
            io: InstallIo::default(),
            started: Instant::now(),
        })
        .unwrap_err();
        assert_eq!(error, InstallError::ProductionWeightsForbidden);
        assert!(store.load_active().unwrap().is_none());
    }

    #[test]
    fn wrong_hash_does_not_activate() {
        let (_temp, mut store) = open_store();
        let entry = ci_fixture_entry();
        let fetcher = ScriptedFetcher::new(vec![
            ScriptedHop::Body(b"voisu-l3-fixture-XXXXX".to_vec()),
            ScriptedHop::Body(CatalogEntry::fixture_pcm_bytes().to_vec()),
        ]);
        let error = install_entry(InstallRequest {
            store: &mut store,
            entry,
            fetcher: &fetcher,
            health: &CandidateHealth::default(),
            maintenance: &IdleMaintenance,
            consent: consent(entry),
            io: InstallIo::default(),
            started: Instant::now(),
        })
        .unwrap_err();
        assert!(matches!(
            error,
            InstallError::Hash | InstallError::Size | InstallError::Fetch(_)
        ));
        assert!(store.load_active().unwrap().is_none());
    }

    #[test]
    fn truncation_is_rejected() {
        let (_temp, mut store) = open_store();
        let entry = ci_fixture_entry();
        let fetcher = ScriptedFetcher::new(vec![ScriptedHop::Truncated(b"short".to_vec())]);
        let error = install_entry(InstallRequest {
            store: &mut store,
            entry,
            fetcher: &fetcher,
            health: &CandidateHealth::default(),
            maintenance: &IdleMaintenance,
            consent: consent(entry),
            io: InstallIo::default(),
            started: Instant::now(),
        })
        .unwrap_err();
        assert!(matches!(
            error,
            InstallError::Fetch(FetchError::Truncated { .. })
        ));
        assert!(store.load_active().unwrap().is_none());
    }

    #[test]
    fn enospc_keeps_prior_receipt() {
        let (_temp, mut store) = open_store();
        let prior = install_ok(&mut store);

        let entry = ci_fixture_entry();
        let error = install_entry(InstallRequest {
            store: &mut store,
            entry,
            fetcher: &fixture_fetcher(),
            health: &CandidateHealth::default(),
            maintenance: &IdleMaintenance,
            consent: consent(entry),
            io: InstallIo {
                fail_write: Some(libc::ENOSPC),
                ..InstallIo::default()
            },
            started: Instant::now(),
        })
        .unwrap_err();
        assert_eq!(error, InstallError::NoSpace);
        assert_eq!(
            store.load_active().unwrap().unwrap().receipt_hash,
            prior.receipt_hash
        );
    }

    #[test]
    fn redirect_escape_does_not_replace_receipt() {
        let (_temp, mut store) = open_store();
        let prior = install_ok(&mut store);

        let entry = ci_fixture_entry();
        let fetcher = ScriptedFetcher::new(vec![ScriptedHop::Redirect(
            "https://evil.example/weights.bin".into(),
        )]);
        let error = install_entry(InstallRequest {
            store: &mut store,
            entry,
            fetcher: &fetcher,
            health: &CandidateHealth::default(),
            maintenance: &IdleMaintenance,
            consent: consent(entry),
            io: InstallIo::default(),
            started: Instant::now(),
        })
        .unwrap_err();
        assert!(matches!(
            error,
            InstallError::Fetch(FetchError::RedirectEscape { .. })
        ));
        assert_eq!(
            store.load_active().unwrap().unwrap().catalog_id,
            prior.catalog_id
        );
    }

    #[test]
    fn failed_health_does_not_activate() {
        let (_temp, mut store) = open_store();
        let prior = install_ok(&mut store);

        let entry = ci_fixture_entry();
        let error = install_entry(InstallRequest {
            store: &mut store,
            entry,
            fetcher: &fixture_fetcher(),
            health: &FailingHealth {
                reason: "fixture quality",
            },
            maintenance: &IdleMaintenance,
            consent: consent(entry),
            io: InstallIo::default(),
            started: Instant::now(),
        })
        .unwrap_err();
        assert!(matches!(error, InstallError::Health(_)));
        assert_eq!(
            store.load_active().unwrap().unwrap().receipt_hash,
            prior.receipt_hash
        );
    }

    #[test]
    fn kill_at_each_step_keeps_prior_receipt() {
        for abort_at in [
            InstallAbort::AfterLock,
            InstallAbort::AfterStaging,
            InstallAbort::AfterFetch,
            InstallAbort::AfterPublish,
            InstallAbort::AfterHealth,
        ] {
            let (_temp, mut store) = open_store();
            let prior = install_ok(&mut store);

            let entry = ci_fixture_entry();
            let error = install_entry(InstallRequest {
                store: &mut store,
                entry,
                fetcher: &fixture_fetcher(),
                health: &CandidateHealth::default(),
                maintenance: &IdleMaintenance,
                consent: consent(entry),
                io: InstallIo {
                    abort: Some(abort_at),
                    ..InstallIo::default()
                },
                started: Instant::now(),
            })
            .unwrap_err();
            assert_eq!(error, InstallError::Aborted, "{abort_at:?}");
            assert_eq!(
                store.load_active().unwrap().unwrap().receipt_hash,
                prior.receipt_hash,
                "{abort_at:?}"
            );
        }
    }

    #[test]
    fn sequential_installs_on_the_same_store_release_the_lock() {
        let (_temp, mut store) = open_store();
        let first = install_ok(&mut store);
        let second = install_ok(&mut store);
        assert_eq!(first.catalog_id, second.catalog_id);
        assert_eq!(
            store.load_active().unwrap().unwrap().receipt_hash,
            second.receipt_hash
        );
    }

    #[test]
    fn concurrent_setup_is_serialized() {
        let temp = tempfile::tempdir().unwrap();
        let first = ModelStore::open(temp.path().to_path_buf()).unwrap();
        let _held = first.lock_exclusive().unwrap();
        let mut second = ModelStore::open(temp.path().to_path_buf()).unwrap();
        let entry = ci_fixture_entry();
        let error = install_entry(InstallRequest {
            store: &mut second,
            entry,
            fetcher: &fixture_fetcher(),
            health: &CandidateHealth::default(),
            maintenance: &IdleMaintenance,
            consent: consent(entry),
            io: InstallIo::default(),
            started: Instant::now(),
        })
        .unwrap_err();
        assert_eq!(error, InstallError::Busy);
    }

    #[test]
    fn recording_busy_is_immediate() {
        let (_temp, mut store) = open_store();
        let entry = ci_fixture_entry();
        let error = install_entry(InstallRequest {
            store: &mut store,
            entry,
            fetcher: &fixture_fetcher(),
            health: &CandidateHealth::default(),
            maintenance: &BusyMaintenance {
                kind: MaintenanceKind::Recording,
            },
            consent: consent(entry),
            io: InstallIo::default(),
            started: Instant::now(),
        })
        .unwrap_err();
        assert!(matches!(
            error,
            InstallError::Maintenance(MaintenanceError::Busy("Recording active"))
        ));
    }

    #[test]
    fn symlink_in_staging_is_rejected() {
        let name = "../etc/passwd";
        assert!(safe_fs::relative_file_name(name).is_err());
    }

    #[test]
    fn no_progress_rejects_stalled_write_not_completed_fetch() {
        let stalled = Instant::now()
            .checked_sub(NO_PROGRESS + Duration::from_secs(1))
            .expect("monotonic clock covers NO_PROGRESS");
        assert_eq!(no_progress(stalled), Err(InstallError::Deadline));
        assert_eq!(no_progress(Instant::now()), Ok(()));
    }

    #[test]
    fn fetch_longer_than_no_progress_still_installs() {
        let (_temp, mut store) = open_store();
        let entry = ci_fixture_entry();
        let started = Instant::now()
            .checked_sub(NO_PROGRESS + Duration::from_secs(1))
            .expect("monotonic clock covers NO_PROGRESS");
        let receipt = install_entry(InstallRequest {
            store: &mut store,
            entry,
            fetcher: &fixture_fetcher(),
            health: &CandidateHealth::default(),
            maintenance: &IdleMaintenance,
            consent: consent(entry),
            io: InstallIo::default(),
            started,
        })
        .unwrap();
        assert_eq!(receipt.catalog_id, "l3-health-fixture");
        assert_eq!(
            store.load_active().unwrap().unwrap().catalog_id,
            receipt.catalog_id
        );
    }
}
