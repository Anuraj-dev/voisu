//! Explicit Consented Install; never production selection or daemon launch.
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::consented_catalog::{CONSENTED_ENTRY, ConsentedCatalogEntry, DownloadPin, WORKER_FILES};
use super::consented_runtime::{
    ConsentedRuntimeBuilder, RuntimeBuild, audit_runtime_tree, executable, sync_tree,
};
use super::progress::{InstallPhase, InstallProgress, ProgressSink};
use super::{
    ArtifactFetcher, FetchError, FetchRequest, INSTALL_DEADLINE, InstallAbort, InstallClock,
    InstallIo, MaintenanceKind, MaintenanceReservation, ModelStore, NO_PROGRESS, SafeFsError,
    StoreError, sha256_hex,
};

#[derive(Debug)]
pub enum ConsentedError {
    Consent,
    Maintenance,
    NoSpace,
    Deadline,
    Aborted,
    Missing,
    Receipt,
    Digest(String),
    Tree(String),
    Build(String),
    Health(String),
    /// Another Local worker holds the GPU (live unit or GPU flock).
    GpuBusy,
    /// Stop/reap was not observed; the publication must remain protected.
    HealthUnreaped,
    Fetch(FetchError),
    Store(StoreError),
    Fs(SafeFsError),
    Io(std::io::Error),
    Leased,
}
impl From<std::io::Error> for ConsentedError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsentedConsent {
    pub text: String,
    pub license_spdx: String,
    pub unix_time: u64,
}
impl ConsentedConsent {
    /// The caller records this only after the user explicitly accepts CONSENT_TEXT.
    pub fn accepted(unix_time: u64) -> Self {
        Self {
            text: CONSENTED_ENTRY.consent_text.into(),
            license_spdx: CONSENTED_ENTRY.license_spdx.into(),
            unix_time,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsentedFilePin {
    pub sha256: String,
    pub bytes: u64,
}
/// Paths are relative to runtime/. Directory sizes are not portable invariants.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, tag = "type", rename_all = "snake_case")]
pub enum ConsentedRuntimeEntry {
    Directory,
    File { bytes: u64, sha256: String },
    Symlink { target: PathBuf },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsentedReceipt {
    pub version: u32,
    pub entry_id: String,
    pub revision: String,
    pub content_hash: String,
    pub consent: ConsentedConsent,
    pub consent_text_sha256: String,
    pub files: BTreeMap<String, ConsentedFilePin>,
    pub runtime_inputs: BTreeMap<String, ConsentedFilePin>,
    // Omit an empty manifest so v1 receipts remain readable for explicit removal.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub runtime_manifest: BTreeMap<String, ConsentedRuntimeEntry>,
    pub python_path: PathBuf,
    pub venv_path: PathBuf,
    pub device: String,
    pub compute_type: String,
    pub receipt_hash: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConsentedLayout {
    pub root: PathBuf,
    pub weights: PathBuf,
    pub health_clip: PathBuf,
    pub python: PathBuf,
    pub venv: PathBuf,
    pub worker: PathBuf,
}
impl ConsentedLayout {
    pub fn at(root: PathBuf) -> Self {
        Self {
            weights: root.join("weights"),
            health_clip: root.join("jfk.wav"),
            python: root.join("runtime/python"),
            venv: root.join("runtime/venv"),
            worker: root.join("worker"),
            root,
        }
    }
    pub fn venv_python(&self) -> PathBuf {
        self.venv.join("bin/python")
    }
}

/// Phase 4 implements the isolated CUDA health run. Installation commits only
/// after this probe accepts the published paths and expected known-audio text.
pub trait ConsentedHealth {
    fn check(&self, layout: &ConsentedLayout) -> Result<(), ConsentedError>;
}

/// Holds an OS shared lease until dropped. No constructible verification token:
/// Phase 4 must retain this handle for the complete worker lifetime.
#[derive(Debug)]
pub struct VerifiedConsentedInstall {
    layout: ConsentedLayout,
    receipt: ConsentedReceipt,
    _lease: ConsentedLease,
    observation: ConsentedObservation,
}
impl VerifiedConsentedInstall {
    pub fn layout(&self) -> &ConsentedLayout {
        &self.layout
    }
    pub fn receipt(&self) -> &ConsentedReceipt {
        &self.receipt
    }
}

/// A daemon-local observation of an already verified installation. It cannot
/// authorize a launch; every launch still obtains a fresh verification token.
#[derive(Clone, Debug)]
pub(crate) struct ConsentedObservation {
    files: Vec<(PathBuf, Option<FileIdentity>)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
    size: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    modified: (i64, i64),
    changed: (i64, i64),
}

fn file_identity(path: &Path) -> std::io::Result<FileIdentity> {
    let metadata = fs::symlink_metadata(path)?;
    Ok(FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        size: metadata.len(),
        mode: metadata.mode(),
        uid: metadata.uid(),
        gid: metadata.gid(),
        modified: (metadata.mtime(), metadata.mtime_nsec()),
        changed: (metadata.ctime(), metadata.ctime_nsec()),
    })
}

impl ConsentedObservation {
    pub(crate) fn after_verification(install: &VerifiedConsentedInstall) -> Self {
        install.observation.clone()
    }

    fn capture(layout: &ConsentedLayout) -> Result<Self, ConsentedError> {
        let root = &layout.root;
        let models = root
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .ok_or(ConsentedError::Receipt)?;
        let mut paths = vec![models.join("private/consented.receipt.json"), root.clone()];
        // Ancestors detect replaced directories and entries added or removed.
        paths.extend(paths.clone().into_iter().flat_map(|path| {
            path.ancestors()
                .skip(1)
                .take_while(|parent| parent.starts_with(models))
                .map(Path::to_path_buf)
                .collect::<Vec<_>>()
        }));
        paths.sort();
        paths.dedup();
        let mut files: Vec<_> = paths
            .into_iter()
            .map(|path| {
                let identity = match file_identity(&path) {
                    Ok(identity) => Some(identity),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(error) => return Err(error),
                };
                Ok((path, identity))
            })
            .collect::<Result<_, std::io::Error>>()?;
        // The security audit includes unmanifested nodes (e.g. Python caches).
        // Record directories before enumeration so a concurrent addition or
        // removal invalidates the observation. Never recurse through links.
        fn observe_children(
            path: &Path,
            files: &mut Vec<(PathBuf, Option<FileIdentity>)>,
        ) -> std::io::Result<()> {
            for child in fs::read_dir(path)? {
                let path = child?.path();
                let identity = file_identity(&path)?;
                let is_dir = identity.mode & libc::S_IFMT == libc::S_IFDIR;
                files.push((path.clone(), Some(identity)));
                if is_dir {
                    observe_children(&path, files)?;
                }
            }
            Ok(())
        }
        observe_children(root, &mut files)?;
        let observation = Self { files };
        if !observation.unchanged() {
            return Err(ConsentedError::Tree(
                "installation changed during observation".into(),
            ));
        }
        Ok(observation)
    }

    pub(crate) fn unchanged(&self) -> bool {
        self.files
            .iter()
            .all(|(path, expected)| match file_identity(path) {
                Ok(actual) => expected.as_ref() == Some(&actual),
                Err(error) => expected.is_none() && error.kind() == std::io::ErrorKind::NotFound,
            })
    }
}

/// Failure observation carries no verification authority. Retry only when the
/// failed tree changes, or when an explicit capture asks for admission again.
/// Decline caching when preflight cannot observe the complete expected tree:
/// a restored install must be retried even if its receipt never changes.
pub(crate) fn observe_with_entry(
    store: &ModelStore,
    entry: &ConsentedCatalogEntry,
) -> Option<ConsentedObservation> {
    load_receipt(store)
        .and_then(|receipt| {
            let layout = validate_receipt(store, entry, &receipt)?;
            ConsentedObservation::capture(&layout)
        })
        .ok()
}

#[cfg(not(test))]
pub(crate) fn observe_consented_install(store: &ModelStore) -> Option<ConsentedObservation> {
    observe_with_entry(store, &CONSENTED_ENTRY)
}

/// The caller keeps this shared lease across transient-unit startup and stop.
#[derive(Debug)]
pub(crate) struct ConsentedLease {
    pub(crate) path: PathBuf,
    _file: File,
}

pub(crate) fn consented_install_lease(
    layout: &ConsentedLayout,
) -> Result<ConsentedLease, ConsentedError> {
    let models = layout
        .root
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .ok_or(ConsentedError::Receipt)?;
    let hash = layout
        .root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(ConsentedError::Receipt)?;
    let path = models
        .join("private")
        .join(format!("consented-{hash}.lease"));
    let file = open_lease(&path)?;
    flock(&file, libc::LOCK_SH)?;
    Ok(ConsentedLease { path, _file: file })
}

pub struct ConsentedInstallRequest<'a> {
    pub store: &'a ModelStore,
    pub consent: ConsentedConsent,
    pub fetcher: &'a dyn ArtifactFetcher,
    pub builder: &'a dyn ConsentedRuntimeBuilder,
    pub health: &'a dyn ConsentedHealth,
    pub maintenance: &'a dyn MaintenanceReservation,
    pub io: InstallIo,
    pub clock: InstallClock,
    /// Observes download bytes and post-download phases; never alters results.
    pub progress: &'a dyn InstallProgress,
}

pub fn install_consented_install(
    request: ConsentedInstallRequest<'_>,
) -> Result<ConsentedReceipt, ConsentedError> {
    install_with_entry(request, &CONSENTED_ENTRY)
}

fn install_with_entry(
    request: ConsentedInstallRequest<'_>,
    entry: &ConsentedCatalogEntry,
) -> Result<ConsentedReceipt, ConsentedError> {
    install_with_receipt_sync(request, entry, sync_dir)
}

fn install_with_receipt_sync(
    request: ConsentedInstallRequest<'_>,
    entry: &ConsentedCatalogEntry,
    receipt_sync: impl Fn(&Path) -> std::io::Result<()>,
) -> Result<ConsentedReceipt, ConsentedError> {
    if !entry.consent_installable
        || request.consent.text.as_bytes() != entry.consent_text.as_bytes()
        || request.consent.license_spdx != entry.license_spdx
    {
        return Err(ConsentedError::Consent);
    }
    if request
        .maintenance
        .acquire()
        .map_err(|_| ConsentedError::Maintenance)?
        != MaintenanceKind::Idle
    {
        return Err(ConsentedError::Maintenance);
    }
    let _lock = request
        .store
        .lock_exclusive()
        .map_err(ConsentedError::Store)?;
    resume_removal(request.store, entry, || Ok(()))?;
    remove_unreceipted_publish(request.store, entry)?;
    abort(&request, InstallAbort::AfterLock)?;
    let required = 2 * (entry.total_download_bytes + entry.estimated_extracted_runtime_bytes);
    if request
        .io
        .available_bytes
        .min(available_space(request.store.root())?)
        < required
    {
        return Err(ConsentedError::NoSpace);
    }
    check_deadline(&request.clock)?;
    let hash = content_hash(entry);
    let published = ConsentedLayout::at(install_root(request.store, entry.id, &hash));
    let staging_parent = request.store.staging_dir("consented");
    super::ensure_private_dir(&staging_parent).map_err(ConsentedError::Fs)?;
    let stage = tempfile::Builder::new()
        .prefix("install-")
        .tempdir_in(staging_parent)?;
    let staging = ConsentedLayout::at(stage.path().to_path_buf());
    for dir in ["weights", "runtime", "worker/voisu_asr", "inputs/wheels"] {
        super::ensure_private_dir(&staging.root.join(dir)).map_err(ConsentedError::Fs)?;
    }
    abort(&request, InstallAbort::AfterStaging)?;
    for pin in entry.weights {
        download(&request, pin, &staging.weights)?;
    }
    download(&request, &(entry.health_clip)(), &staging.root)?;
    download(&request, &entry.python, &staging.root.join("inputs"))?;
    for pin in entry.wheels {
        download(&request, pin, &staging.root.join("inputs/wheels"))?;
    }
    for (name, bytes) in WORKER_FILES {
        let path = staging.worker.join(name);
        write_new(
            path.parent().expect("worker parent"),
            path.file_name()
                .expect("worker name")
                .to_str()
                .expect("UTF-8"),
            bytes,
        )?;
    }
    abort(&request, InstallAbort::AfterFetch)?;
    request.progress.phase(InstallPhase::BuildingRuntime);
    request.builder.build(&RuntimeBuild {
        staging: &staging,
        published: &published,
        entry,
        clock: &request.clock,
    })?;
    check_deadline(&request.clock)?;
    fs::remove_dir_all(staging.root.join("inputs"))?;
    for name in ["build-home", "build-tmp"] {
        let path = staging.root.join(name);
        if path.exists() {
            fs::remove_dir_all(path)?;
        }
    }
    audit_runtime_tree(&staging.root)?;
    executable(&staging.venv_python())?;
    let mut receipt = make_receipt(entry, request.consent.clone(), &published);
    receipt.runtime_manifest = runtime_manifest(&staging.root.join("runtime"))?;
    receipt.receipt_hash = receipt_hash(&receipt)?;
    verify_files(&staging, &receipt)?;
    let existed = match fs::symlink_metadata(&published.root) {
        Ok(_) => {
            // Rebuilds produce different .pyc bytes (staging paths/timestamps).
            // Reuse only a previously receipted tree, checked against its own manifest.
            let prior = load_receipt(request.store)?;
            validate_receipt(request.store, entry, &prior)?;
            verify_layout(&published, &prior)?;
            receipt.runtime_manifest = prior.runtime_manifest;
            receipt.receipt_hash = receipt_hash(&receipt)?;
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    if !existed {
        super::ensure_private_dir(published.root.parent().expect("version parent"))
            .map_err(ConsentedError::Fs)?;
        sync_tree(&staging.root)?;
        super::safe_fs::durable_rename(&staging.root, &published.root)
            .map_err(ConsentedError::Fs)?;
    }
    // Once receipt replacement is attempted, rollback must be durable before
    // this tree may be deleted. An uncertain rollback retains both trees.
    let mut may_reference_new = false;
    let result = (|| {
        abort(&request, InstallAbort::AfterPublish)?;
        verify_layout_full(&published, &receipt)?;
        request.progress.phase(InstallPhase::CheckingGpu);
        request.health.check(&published)?;
        verify_layout(&published, &receipt)?;
        check_deadline(&request.clock)?;
        abort(&request, InstallAbort::AfterHealth)?;
        store_receipt_with_sync(
            request.store,
            &receipt,
            &mut may_reference_new,
            receipt_sync,
        )?;
        Ok(receipt)
    })();
    if result.is_err()
        && !existed
        && !may_reference_new
        && !matches!(result, Err(ConsentedError::HealthUnreaped))
    {
        // Never roll back beneath a health or daemon worker. Even an ordinary
        // failure may leave a unit alive; unknown stop state retains the tree.
        let lease = lease_file(request.store, &hash)?;
        match flock(&lease, libc::LOCK_EX) {
            Ok(()) => {
                fs::remove_dir_all(&published.root)?;
                File::open(published.root.parent().expect("parent"))?.sync_all()?;
            }
            Err(ConsentedError::Leased) => {}
            Err(error) => return Err(error),
        }
    }
    result
}

/// Per-launch admission hashes model/fixture/worker and checks runtime metadata.
/// It does not detect same-size runtime edits; use the full verifier for that.
/// Neither receipt hash nor filesystem audit defends against a malicious same uid.
pub fn verify_consented_install(
    store: &ModelStore,
) -> Result<VerifiedConsentedInstall, ConsentedError> {
    verify_with_entry(store, &CONSENTED_ENTRY)
}
pub(crate) fn verify_with_entry(
    store: &ModelStore,
    entry: &ConsentedCatalogEntry,
) -> Result<VerifiedConsentedInstall, ConsentedError> {
    verify_with_entry_notifying(store, entry, || {})
}

/// Notify only after receipt and install preflight succeeds, before the costly
/// file verification. Missing or inadmissible installs never enter this stage.
pub(crate) fn verify_with_entry_notifying(
    store: &ModelStore,
    entry: &ConsentedCatalogEntry,
    on_verifying: impl FnOnce(),
) -> Result<VerifiedConsentedInstall, ConsentedError> {
    verify_with_entry_mode(store, entry, false, on_verifying)
}

/// Re-hash every runtime file and reject all unmanifested runtime entries.
/// Uses the same lease and lock contract as per-launch admission.
pub fn verify_consented_install_full(
    store: &ModelStore,
) -> Result<VerifiedConsentedInstall, ConsentedError> {
    verify_with_entry_mode(store, &CONSENTED_ENTRY, true, || {})
}

fn verify_with_entry_mode(
    store: &ModelStore,
    entry: &ConsentedCatalogEntry,
    full: bool,
    on_verifying: impl FnOnce(),
) -> Result<VerifiedConsentedInstall, ConsentedError> {
    let _lock = store.lock_exclusive().map_err(ConsentedError::Store)?;
    resume_removal(store, entry, || Ok(()))?;
    let receipt_identity = file_identity(&receipt_path(store)).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ConsentedError::Missing
        } else {
            ConsentedError::Io(error)
        }
    })?;
    let receipt = load_receipt(store)?;
    let layout = validate_receipt(store, entry, &receipt)?;
    let lease = consented_install_lease(&layout)?;
    let observation = ConsentedObservation::capture(&layout)?;
    on_verifying();
    if full {
        verify_layout_full(&layout, &receipt)?;
    } else {
        verify_layout(&layout, &receipt)?;
    }
    if file_identity(&receipt_path(store))? != receipt_identity || !observation.unchanged() {
        return Err(ConsentedError::Tree(
            "installation changed during verification".into(),
        ));
    }
    Ok(VerifiedConsentedInstall {
        observation,
        layout,
        receipt,
        _lease: lease,
    })
}

/// Result decided under the store lock, including recovery and orphan cleanup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConsentedRemoveOutcome {
    Removed,
    Absent,
}

pub fn remove_consented_install(
    store: &ModelStore,
) -> Result<ConsentedRemoveOutcome, ConsentedError> {
    remove_with_entry(store, &CONSENTED_ENTRY)
}

/// Whether a Consented Install receipt file is present. Does not parse it.
#[must_use]
pub(crate) fn consented_receipt_exists(store: &ModelStore) -> bool {
    receipt_path(store).is_file()
}
fn remove_with_entry(
    store: &ModelStore,
    entry: &ConsentedCatalogEntry,
) -> Result<ConsentedRemoveOutcome, ConsentedError> {
    remove_with_delete_hook(store, entry, || Ok(()))
}
fn remove_with_delete_hook(
    store: &ModelStore,
    entry: &ConsentedCatalogEntry,
    after_receipt_delete: impl FnOnce() -> Result<(), ConsentedError>,
) -> Result<ConsentedRemoveOutcome, ConsentedError> {
    let _lock = store.lock_exclusive().map_err(ConsentedError::Store)?;
    let resumed = resume_removal(store, entry, || Ok(()))?;
    let cleaned = remove_unreceipted_publish(store, entry)?;
    // Cleanup already parsed any present receipt. The lock keeps this absence
    // check and the completed recovery work in the same namespace snapshot.
    if !super::consented_receipt_exists(store) {
        return Ok(if resumed || cleaned {
            ConsentedRemoveOutcome::Removed
        } else {
            ConsentedRemoveOutcome::Absent
        });
    }
    let receipt = load_receipt(store)?;
    validate_removal_target(entry, &receipt)?;
    let lease = lease_file(store, &receipt.content_hash)?;
    flock(&lease, libc::LOCK_EX)?;
    // Journal the exact target durably before withdrawing its receipt.
    write_record(
        store,
        "consented.removal.json",
        &serde_json::to_vec(&receipt).map_err(|_| ConsentedError::Receipt)?,
    )?;
    drop(lease);
    resume_removal(store, entry, after_receipt_delete)?;
    Ok(ConsentedRemoveOutcome::Removed)
}

fn validate_removal_target(
    entry: &ConsentedCatalogEntry,
    receipt: &ConsentedReceipt,
) -> Result<(), ConsentedError> {
    // Old pins and partially removed runtimes must remain uninstallable.
    if receipt.entry_id != entry.id
        || receipt.content_hash.len() != 64
        || !receipt
            .content_hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || receipt.receipt_hash != receipt_hash(receipt)?
    {
        return Err(ConsentedError::Receipt);
    }
    Ok(())
}

// Caller holds the store lock. A lease blocks both initial and resumed removal.
fn resume_removal(
    store: &ModelStore,
    entry: &ConsentedCatalogEntry,
    after_receipt_delete: impl FnOnce() -> Result<(), ConsentedError>,
) -> Result<bool, ConsentedError> {
    let bytes = match read_record(store, "consented.removal.json") {
        Err(ConsentedError::Missing) => return Ok(false),
        other => other?,
    };
    let receipt: ConsentedReceipt =
        serde_json::from_slice(&bytes).map_err(|_| ConsentedError::Receipt)?;
    validate_removal_target(entry, &receipt)?;
    let lease = lease_file(store, &receipt.content_hash)?;
    flock(&lease, libc::LOCK_EX)?;
    match load_receipt(store) {
        Ok(active) if active == receipt => fs::remove_file(receipt_path(store))?,
        Err(ConsentedError::Missing) => {}
        Ok(_) => return Err(ConsentedError::Receipt),
        Err(error) => return Err(error),
    }
    // Stabilize receipt withdrawal (including a prior uncertain rollback)
    // before deleting its target. A failed sync leaves the tree and journal.
    sync_dir(&store.root().join("private"))?;
    after_receipt_delete()?;
    let root = install_root(store, entry.id, &receipt.content_hash);
    if super::safe_fs::dir_present(&root).map_err(ConsentedError::Fs)? {
        fs::remove_dir_all(&root)?;
    }
    if super::safe_fs::dir_present(root.parent().expect("parent")).map_err(ConsentedError::Fs)? {
        sync_dir(root.parent().expect("parent"))?;
    }
    fs::remove_file(store.root().join("private/consented.removal.json"))?;
    sync_dir(&store.root().join("private"))?;
    Ok(true)
}

/// Only the current entry's exact publication can be an orphan. The store
/// lock serializes namespace changes; the lease excludes a surviving unit.
fn remove_unreceipted_publish(
    store: &ModelStore,
    entry: &ConsentedCatalogEntry,
) -> Result<bool, ConsentedError> {
    remove_unreceipted_publish_with_sync(store, entry, sync_dir)
}
fn remove_unreceipted_publish_with_sync(
    store: &ModelStore,
    entry: &ConsentedCatalogEntry,
    private_sync: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<bool, ConsentedError> {
    // An earlier receipt replacement and rollback may both have failed fsync.
    // Stabilize the visible receipt before treating its other target as orphaned.
    // Caller holds the store lock; failure must retain the publication.
    private_sync(&store.root().join("private"))?;
    let hash = content_hash(entry);
    match load_receipt(store) {
        Ok(receipt) if receipt.content_hash == hash => return Ok(false),
        Ok(_) | Err(ConsentedError::Missing) => {}
        Err(error) => return Err(error),
    }
    let root = install_root(store, entry.id, &hash);
    if super::safe_fs::dir_present(&root).map_err(ConsentedError::Fs)? {
        let lease = lease_file(store, &hash)?;
        flock(&lease, libc::LOCK_EX)?;
        fs::remove_dir_all(&root)?;
        sync_dir(root.parent().expect("version parent"))?;
        return Ok(true);
    }
    Ok(false)
}

fn lease_file(store: &ModelStore, hash: &str) -> Result<File, ConsentedError> {
    open_lease(
        &store
            .root()
            .join("private")
            .join(format!("consented-{hash}.lease")),
    )
}
fn open_lease(path: &Path) -> Result<File, ConsentedError> {
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?)
}
fn flock(file: &File, operation: i32) -> Result<(), ConsentedError> {
    flock_with_retry(
        || {
            // SAFETY: flock borrows a valid owned fd; it does not dereference memory.
            if unsafe { libc::flock(file.as_raw_fd(), operation | libc::LOCK_NB) } == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        },
        Instant::now,
        std::thread::sleep,
    )
}
fn flock_with_retry(
    mut attempt: impl FnMut() -> std::io::Result<()>,
    now: impl Fn() -> Instant,
    mut sleep: impl FnMut(Duration),
) -> Result<(), ConsentedError> {
    // Retry EAGAIN/EWOULDBLOCK: a fork-inherited duplicate descriptor can hold the lock between fork and exec.
    const BUDGET: Duration = Duration::from_millis(200);
    const PAUSE: Duration = Duration::from_millis(8);
    let deadline = now() + BUDGET;
    loop {
        let error = match attempt() {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        match error.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(errno) if errno == libc::EAGAIN || errno == libc::EWOULDBLOCK => {
                if now() >= deadline {
                    return Err(ConsentedError::Leased);
                }
                sleep(PAUSE);
            }
            _ => return Err(error.into()),
        }
    }
}
fn install_root(store: &ModelStore, id: &str, hash: &str) -> PathBuf {
    store.root().join("consented").join(id).join(hash)
}
fn receipt_path(store: &ModelStore) -> PathBuf {
    store.root().join("private/consented.receipt.json")
}
fn pin(pin: &DownloadPin) -> ConsentedFilePin {
    ConsentedFilePin {
        sha256: pin.sha256.into(),
        bytes: pin.bytes,
    }
}
fn make_receipt(
    entry: &ConsentedCatalogEntry,
    consent: ConsentedConsent,
    layout: &ConsentedLayout,
) -> ConsentedReceipt {
    let mut files = BTreeMap::new();
    for item in entry.weights {
        files.insert(format!("weights/{}", item.name), pin(item));
    }
    files.insert("jfk.wav".into(), pin(&(entry.health_clip)()));
    for (name, bytes) in WORKER_FILES {
        files.insert(
            format!("worker/{name}"),
            ConsentedFilePin {
                sha256: sha256_hex(bytes),
                bytes: bytes.len() as u64,
            },
        );
    }
    let mut runtime_inputs = BTreeMap::new();
    runtime_inputs.insert(entry.python.name.into(), pin(&entry.python));
    for item in entry.wheels {
        runtime_inputs.insert(item.name.into(), pin(item));
    }
    ConsentedReceipt {
        version: 2,
        entry_id: entry.id.into(),
        revision: entry.revision.into(),
        content_hash: content_hash(entry),
        consent_text_sha256: sha256_hex(consent.text.as_bytes()),
        consent,
        files,
        runtime_inputs,
        runtime_manifest: BTreeMap::new(),
        python_path: layout.python.join("bin/python3"),
        venv_path: layout.venv_python(),
        device: "cuda:0".into(),
        compute_type: "int8_float16".into(),
        receipt_hash: String::new(),
    }
}
fn content_hash(entry: &ConsentedCatalogEntry) -> String {
    let mut bytes = format!(
        "consented-builder-v2\n{}\n{}\ncuda:0\nint8_float16\n",
        entry.id, entry.revision
    );
    for item in entry
        .weights
        .iter()
        .chain(std::iter::once(&entry.python))
        .chain(entry.wheels)
    {
        bytes.push_str(&format!("{}:{}:{}\n", item.name, item.bytes, item.sha256));
    }
    bytes.push_str((entry.health_clip)().sha256);
    for (name, data) in WORKER_FILES {
        bytes.push_str(&format!("{name}:{}\n", sha256_hex(data)));
    }
    sha256_hex(bytes.as_bytes())
}
fn receipt_hash(receipt: &ConsentedReceipt) -> Result<String, ConsentedError> {
    let mut body = receipt.clone();
    body.receipt_hash.clear();
    Ok(sha256_hex(
        &serde_json::to_vec(&body).map_err(|_| ConsentedError::Receipt)?,
    ))
}
fn validate_receipt(
    store: &ModelStore,
    entry: &ConsentedCatalogEntry,
    receipt: &ConsentedReceipt,
) -> Result<ConsentedLayout, ConsentedError> {
    if !entry.consent_installable
        || receipt.consent.text != entry.consent_text
        || receipt.consent.license_spdx != entry.license_spdx
    {
        return Err(ConsentedError::Receipt);
    }
    let layout = ConsentedLayout::at(install_root(store, entry.id, &content_hash(entry)));
    let mut expected = make_receipt(entry, receipt.consent.clone(), &layout);
    validate_runtime_manifest(&receipt.runtime_manifest)?;
    expected.runtime_manifest = receipt.runtime_manifest.clone();
    expected.receipt_hash = receipt_hash(&expected)?;
    if *receipt != expected {
        return Err(ConsentedError::Receipt);
    }
    // Inspect ancestors before following any receipt-controlled runtime paths.
    // Ancestors are opened without following links; the full runtime audit
    // permits only internal relative links (e.g. venv lib64 -> lib).
    if !super::safe_fs::dir_present(&layout.root).map_err(ConsentedError::Fs)? {
        return Err(ConsentedError::Missing);
    }
    Ok(layout)
}
const MAX_RECORD_BYTES: u64 = 32 * 1024 * 1024;

fn read_record(store: &ModelStore, name: &str) -> Result<Vec<u8>, ConsentedError> {
    // Tens of thousands of Python/CUDA files need more than the old 128 KiB cap.
    super::safe_fs::read_existing_file(&store.root().join("private"), name, MAX_RECORD_BYTES)
        .map_err(|error| {
            if error == SafeFsError::NotFound {
                ConsentedError::Missing
            } else {
                ConsentedError::Fs(error)
            }
        })
}
fn load_receipt(store: &ModelStore) -> Result<ConsentedReceipt, ConsentedError> {
    serde_json::from_slice(&read_record(store, "consented.receipt.json")?)
        .map_err(|_| ConsentedError::Receipt)
}
fn sync_dir(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}
fn stage_record(
    store: &ModelStore,
    bytes: &[u8],
) -> Result<tempfile::NamedTempFile, ConsentedError> {
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(ConsentedError::Receipt);
    }
    let mut staged = tempfile::Builder::new()
        .prefix(".consented-record-")
        .tempfile_in(store.root().join("private"))?;
    staged.write_all(bytes)?;
    staged.as_file().sync_all()?;
    Ok(staged)
}
fn write_record(store: &ModelStore, name: &str, bytes: &[u8]) -> Result<(), ConsentedError> {
    stage_record(store, bytes)?
        .persist(store.root().join("private").join(name))
        .map_err(|error| ConsentedError::Io(error.error))?;
    sync_dir(&store.root().join("private"))?;
    Ok(())
}
#[cfg(test)]
fn store_receipt(store: &ModelStore, receipt: &ConsentedReceipt) -> Result<(), ConsentedError> {
    store_receipt_with_sync(store, receipt, &mut false, sync_dir)
}
fn store_receipt_with_sync(
    store: &ModelStore,
    receipt: &ConsentedReceipt,
    may_reference_new: &mut bool,
    sync: impl Fn(&Path) -> std::io::Result<()>,
) -> Result<(), ConsentedError> {
    let parent = store.root().join("private");
    let prior = match read_record(store, "consented.receipt.json") {
        Ok(bytes) => Some(stage_record(store, &bytes)?),
        Err(ConsentedError::Missing) => None,
        Err(error) => return Err(error),
    };
    let staged = stage_record(
        store,
        &serde_json::to_vec_pretty(receipt).map_err(|_| ConsentedError::Receipt)?,
    )?;
    staged
        .persist(receipt_path(store))
        .map_err(|error| ConsentedError::Io(error.error))?;
    *may_reference_new = true;
    if let Err(error) = sync(&parent) {
        // Restore the old receipt, but retain the new tree if either rollback
        // rename/unlink or rollback fsync fails: disk may still reference it.
        let restored = match prior {
            Some(prior) => prior
                .persist(receipt_path(store))
                .map(|_| ())
                .map_err(|error| error.error),
            None => fs::remove_file(receipt_path(store)),
        };
        if restored.is_ok() && sync(&parent).is_ok() {
            *may_reference_new = false;
        }
        return Err(error.into());
    }
    Ok(())
}
fn verify_layout(
    layout: &ConsentedLayout,
    receipt: &ConsentedReceipt,
) -> Result<(), ConsentedError> {
    audit_runtime_tree(&layout.root)?;
    audit_runtime_tree(&layout.root.join("runtime"))?;
    if !layout.python.is_dir() || !layout.venv.is_dir() {
        return Err(ConsentedError::Tree("missing runtime directory".into()));
    }
    executable(&layout.venv_python())?;
    verify_runtime_manifest(layout, &receipt.runtime_manifest)?;
    verify_files(layout, receipt)
}
fn verify_layout_full(
    layout: &ConsentedLayout,
    receipt: &ConsentedReceipt,
) -> Result<(), ConsentedError> {
    verify_layout(layout, receipt)?;
    if runtime_manifest(&layout.root.join("runtime"))? != receipt.runtime_manifest {
        return Err(ConsentedError::Tree(
            "runtime manifest digest or entry mismatch".into(),
        ));
    }
    Ok(())
}
fn runtime_manifest(
    root: &Path,
) -> Result<BTreeMap<String, ConsentedRuntimeEntry>, ConsentedError> {
    fn collect(
        root: &Path,
        dir: &Path,
        manifest: &mut BTreeMap<String, ConsentedRuntimeEntry>,
    ) -> Result<(), ConsentedError> {
        for child in fs::read_dir(dir)? {
            let path = child?.path();
            let name = path
                .strip_prefix(root)
                .expect("runtime child")
                .to_str()
                .ok_or_else(|| ConsentedError::Tree("non-UTF-8 runtime path".into()))?
                .to_owned();
            let meta = fs::symlink_metadata(&path)?;
            let entry = if meta.is_dir() {
                collect(root, &path, manifest)?;
                ConsentedRuntimeEntry::Directory
            } else if meta.is_file() {
                ConsentedRuntimeEntry::File {
                    bytes: meta.len(),
                    sha256: hash_runtime_file(&path)?,
                }
            } else if meta.file_type().is_symlink() {
                ConsentedRuntimeEntry::Symlink {
                    target: fs::read_link(&path)?,
                }
            } else {
                return Err(ConsentedError::Tree(format!(
                    "special runtime node: {name}"
                )));
            };
            manifest.insert(name, entry);
        }
        Ok(())
    }
    let mut manifest = BTreeMap::new();
    collect(root, root, &mut manifest)?;
    Ok(manifest)
}
fn hash_runtime_file(path: &Path) -> Result<String, ConsentedError> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let mut hash = Sha256::new();
    let mut buf = [0_u8; 64 * 1024];
    loop {
        let len = file.read(&mut buf)?;
        if len == 0 {
            break;
        }
        hash.update(&buf[..len]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn validate_runtime_manifest(
    manifest: &BTreeMap<String, ConsentedRuntimeEntry>,
) -> Result<(), ConsentedError> {
    for required in ["python", "venv", "venv/bin"] {
        if manifest.get(required) != Some(&ConsentedRuntimeEntry::Directory) {
            return Err(ConsentedError::Receipt);
        }
    }
    if !matches!(
        manifest.get("venv/bin/python"),
        Some(ConsentedRuntimeEntry::File { .. })
    ) {
        return Err(ConsentedError::Receipt);
    }
    for (name, entry) in manifest {
        let path = Path::new(name);
        if name.is_empty()
            || path
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err(ConsentedError::Receipt);
        }
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            && manifest.get(parent.to_str().ok_or(ConsentedError::Receipt)?)
                != Some(&ConsentedRuntimeEntry::Directory)
        {
            return Err(ConsentedError::Receipt);
        }
        match entry {
            ConsentedRuntimeEntry::File { sha256, .. }
                if sha256.len() != 64
                    || !sha256
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) =>
            {
                return Err(ConsentedError::Receipt);
            }
            ConsentedRuntimeEntry::Symlink { target } if target.is_absolute() => {
                return Err(ConsentedError::Receipt);
            }
            _ => {}
        }
    }
    Ok(())
}
fn verify_runtime_manifest(
    layout: &ConsentedLayout,
    manifest: &BTreeMap<String, ConsentedRuntimeEntry>,
) -> Result<(), ConsentedError> {
    validate_runtime_manifest(manifest)?;
    let runtime = layout.root.join("runtime");
    for (name, expected) in manifest {
        let path = runtime.join(name);
        let meta = fs::symlink_metadata(&path)?;
        let matches = match expected {
            ConsentedRuntimeEntry::Directory => meta.is_dir(),
            ConsentedRuntimeEntry::File { bytes, .. } => meta.is_file() && meta.len() == *bytes,
            ConsentedRuntimeEntry::Symlink { target } => {
                meta.file_type().is_symlink() && fs::read_link(&path)? == *target
            }
        };
        if !matches {
            return Err(ConsentedError::Tree(format!(
                "runtime metadata mismatch: {name}"
            )));
        }
        // Executable entrypoints and import/metadata roots must have exactly the
        // recorded children: extras here can change command/import resolution.
        // Deeper extras (e.g. __pycache__ from health/worker runs) are allowed on
        // launch, but still audited for ownership/mode/links. Full verification
        // rejects all extras and hashes every file in the tree.
        if matches!(expected, ConsentedRuntimeEntry::Directory)
            && (name == "venv/bin" || path.file_name().is_some_and(|leaf| leaf == "site-packages"))
        {
            for child in fs::read_dir(&path)? {
                let child = child?;
                let leaf = child.file_name();
                let leaf = leaf
                    .to_str()
                    .ok_or_else(|| ConsentedError::Tree("non-UTF-8 runtime path".into()))?;
                if !manifest.contains_key(&format!("{name}/{leaf}")) {
                    return Err(ConsentedError::Tree(format!(
                        "extra runtime entry: {name}/{leaf}"
                    )));
                }
            }
        }
    }
    Ok(())
}
fn verify_files(
    layout: &ConsentedLayout,
    receipt: &ConsentedReceipt,
) -> Result<(), ConsentedError> {
    for (name, expected) in &receipt.files {
        let path = layout.root.join(name);
        let parent = path.parent().ok_or(ConsentedError::Receipt)?;
        let leaf = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(ConsentedError::Receipt)?;
        let mut file =
            super::safe_fs::open_existing_file(parent, leaf).map_err(ConsentedError::Fs)?;
        if file.metadata()?.len() != expected.bytes {
            return Err(ConsentedError::Digest(name.clone()));
        }
        let mut hash = Sha256::new();
        let mut buf = [0_u8; 64 * 1024];
        loop {
            let len = file.read(&mut buf)?;
            if len == 0 {
                break;
            }
            hash.update(&buf[..len]);
        }
        let actual = format!("{:x}", hash.finalize());
        if actual != expected.sha256 {
            return Err(ConsentedError::Digest(name.clone()));
        }
    }
    Ok(())
}
fn download(
    request: &ConsentedInstallRequest<'_>,
    pin: &DownloadPin,
    dir: &Path,
) -> Result<(), ConsentedError> {
    super::relative_file_name(pin.name).map_err(ConsentedError::Fs)?;
    check_deadline(&request.clock)?;
    let mut pending = super::safe_fs::PendingDownload::new(dir).map_err(ConsentedError::Fs)?;
    if let Some(errno) = request.io.fail_write {
        return Err(if errno == libc::ENOSPC {
            ConsentedError::NoSpace
        } else {
            std::io::Error::from_raw_os_error(errno).into()
        });
    }
    let map_fetch = |error| match error {
        FetchError::Deadline | FetchError::NoProgress => ConsentedError::Deadline,
        FetchError::NoSpace => ConsentedError::NoSpace,
        other => ConsentedError::Fetch(other),
    };
    let fetched = request
        .fetcher
        .fetch_to(
            &FetchRequest {
                url: pin.url.into(),
                allowed_hosts: pin.hosts.iter().map(|host| (*host).into()).collect(),
                expected_bytes: pin.bytes,
            },
            &mut ProgressSink::new(pending.file_mut(), request.progress),
        )
        .map_err(map_fetch)?;
    check_deadline(&request.clock)?;
    if !fetched.matches_pin(pin.sha256, pin.bytes) {
        return Err(ConsentedError::Digest(pin.name.into()));
    }
    let before = request.clock.elapsed();
    pending
        .file_mut()
        .sync_all()
        .map_err(|error| map_fetch(super::fetch::io_err(error)))?;
    if request.clock.elapsed().saturating_sub(before) > NO_PROGRESS {
        return Err(ConsentedError::Deadline);
    }
    check_deadline(&request.clock)?;
    pending.publish(pin.name).map_err(ConsentedError::Fs)
}
fn write_new(dir: &Path, name: &str, data: &[u8]) -> Result<(), ConsentedError> {
    super::relative_file_name(name).map_err(ConsentedError::Fs)?;
    // The production weights writer deliberately forbids archives. This path
    // needs verified tarballs/wheels; retain exclusive, private, no-follow opens.
    if !super::safe_fs::dir_present(dir).map_err(ConsentedError::Fs)? {
        return Err(ConsentedError::Missing);
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join(name))?;
    super::safe_fs::durable_write(&mut file, data).map_err(ConsentedError::Fs)
}
pub(super) fn check_deadline(clock: &InstallClock) -> Result<(), ConsentedError> {
    if clock.elapsed() > INSTALL_DEADLINE {
        Err(ConsentedError::Deadline)
    } else {
        Ok(())
    }
}
fn abort(request: &ConsentedInstallRequest<'_>, point: InstallAbort) -> Result<(), ConsentedError> {
    if request.io.abort == Some(point) {
        Err(ConsentedError::Aborted)
    } else {
        check_deadline(&request.clock)
    }
}
fn available_space(root: &Path) -> Result<u64, ConsentedError> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(root.as_os_str().as_bytes())
        .map_err(|_| ConsentedError::Tree("store NUL".into()))?;
    let mut info = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: path is NUL terminated, info points to writable statvfs storage.
    if unsafe { libc::statvfs(path.as_ptr(), info.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: statvfs initialized info on success above.
    let info = unsafe { info.assume_init() };
    Ok(info.f_bavail.saturating_mul(info.f_frsize))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn consented_tree_audit_rejects_traversal_absolute_links_and_setuid() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("runtime");
        fs::create_dir(&root).unwrap();
        assert!(audit_runtime_tree(&root).is_ok());
        assert!(audit_runtime_tree(&root.join("..")).is_err());
        symlink("/etc/passwd", root.join("escape")).unwrap();
        assert!(audit_runtime_tree(&root).is_err());
        fs::remove_file(root.join("escape")).unwrap();
        fs::write(root.join("bad"), b"x").unwrap();
        fs::set_permissions(root.join("bad"), fs::Permissions::from_mode(0o4700)).unwrap();
        assert!(audit_runtime_tree(&root).is_err());
    }

    #[test]
    fn flock_retries_contention_then_succeeds() {
        let start = Instant::now();
        let elapsed = Cell::new(Duration::ZERO);
        let mut attempts = 0;
        let mut pauses = 0;
        flock_with_retry(
            || {
                attempts += 1;
                if attempts <= 3 {
                    Err(std::io::Error::from_raw_os_error(libc::EAGAIN))
                } else {
                    Ok(())
                }
            },
            || start + elapsed.get(),
            |pause| {
                assert_eq!(pause, Duration::from_millis(8));
                pauses += 1;
                elapsed.set(elapsed.get() + pause);
            },
        )
        .unwrap();
        assert_eq!(attempts, 4);
        assert_eq!(pauses, 3);
        assert_eq!(elapsed.get(), Duration::from_millis(24));
    }

    #[test]
    fn flock_contention_exhausts_budget() {
        let start = Instant::now();
        let elapsed = Cell::new(Duration::ZERO);
        let mut attempts = 0;
        let mut pauses = 0;
        assert!(matches!(
            flock_with_retry(
                || {
                    attempts += 1;
                    Err(std::io::Error::from_raw_os_error(libc::EWOULDBLOCK))
                },
                || start + elapsed.get(),
                |pause| {
                    assert_eq!(pause, Duration::from_millis(8));
                    pauses += 1;
                    elapsed.set(elapsed.get() + pause);
                },
            ),
            Err(ConsentedError::Leased)
        ));
        assert_eq!(attempts, 26);
        assert_eq!(pauses, 25);
        assert_eq!(elapsed.get(), Duration::from_millis(200));
    }

    #[test]
    fn flock_retries_eintr_without_sleep_or_contention_deadline() {
        let start = Instant::now();
        let attempts = Cell::new(0);
        flock_with_retry(
            || {
                attempts.set(attempts.get() + 1);
                if attempts.get() <= 2 {
                    Err(std::io::Error::from_raw_os_error(libc::EINTR))
                } else {
                    Ok(())
                }
            },
            || {
                if attempts.get() == 0 {
                    start
                } else {
                    start + Duration::from_secs(1)
                }
            },
            |_| panic!("EINTR must retry immediately"),
        )
        .unwrap();
        assert_eq!(attempts.get(), 3);
    }

    #[test]
    fn flock_other_errno_is_io_without_retry() {
        let mut attempts = 0;
        assert!(matches!(
            flock_with_retry(
                || {
                    attempts += 1;
                    Err(std::io::Error::from_raw_os_error(libc::EBADF))
                },
                Instant::now,
                |_| panic!("other errnos must return immediately"),
            ),
            Err(ConsentedError::Io(error)) if error.raw_os_error() == Some(libc::EBADF)
        ));
        assert_eq!(attempts, 1);
    }

    #[test]
    fn flock_held_on_another_open_file_description_is_leased() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("lease");
        let holder = open_lease(&path).unwrap();
        flock(&holder, libc::LOCK_EX).unwrap();
        let waiter = open_lease(&path).unwrap();
        assert!(matches!(
            flock(&waiter, libc::LOCK_EX),
            Err(ConsentedError::Leased)
        ));
    }
}

#[cfg(test)]
pub(crate) mod install_tests {
    use super::super::installer::{BusyMaintenance, IdleMaintenance};
    use super::super::{CatalogEntry, ScriptedFetcher, ScriptedHop, SilentProgress};
    use super::*;
    use std::cell::Cell;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::time::Duration;

    const DATA: &[u8] = b"voisu-l3-fixture-model";
    const SHA: &str = "0baba5aced9189d1fa6b1d464744c379d31c6874564d2d05dc40f7c305a0c96f";
    const INPUT: DownloadPin = DownloadPin {
        name: "python.tar.gz",
        url: "https://fixtures.voisu.test/python.tar.gz",
        sha256: SHA,
        bytes: DATA.len() as u64,
        hosts: &["fixtures.voisu.test"],
        requirement: None,
    };
    const WEIGHTS: &[DownloadPin] = &[DownloadPin {
        name: "model.bin",
        ..INPUT
    }];
    const WHEELS: &[DownloadPin] = &[DownloadPin {
        name: "fixture-1-py3-none-any.whl",
        requirement: Some("fixture==1"),
        ..INPUT
    }];
    fn fixture_clip() -> DownloadPin {
        DownloadPin {
            name: "jfk.wav",
            sha256: "1af47a487fa2083c5924dc26e21e16c606458d55bf45d840ca171323838f72bf",
            bytes: 20,
            ..INPUT
        }
    }
    pub(crate) fn entry() -> ConsentedCatalogEntry {
        ConsentedCatalogEntry {
            id: "consented-test",
            revision: "1",
            license_spdx: "MIT",
            consent_installable: true,
            weights: WEIGHTS,
            python: INPUT,
            wheels: WHEELS,
            health_clip: fixture_clip,
            total_download_bytes: 83,
            estimated_extracted_runtime_bytes: 128,
            consent_text: "83 bytes; MIT; explicit fixture consent",
        }
    }
    fn consent() -> ConsentedConsent {
        ConsentedConsent {
            text: entry().consent_text.into(),
            license_spdx: "MIT".into(),
            unix_time: 42,
        }
    }
    fn store() -> (tempfile::TempDir, ModelStore) {
        let temp = tempfile::tempdir().unwrap();
        let root =
            super::super::models_dir_from_state(Some(temp.path().as_os_str().to_owned()), None)
                .unwrap();
        let store = ModelStore::open(root).unwrap();
        (temp, store)
    }
    fn fetcher() -> ScriptedFetcher {
        ScriptedFetcher::new(vec![
            ScriptedHop::Body(DATA.to_vec()),
            ScriptedHop::Body(CatalogEntry::fixture_pcm_bytes().to_vec()),
            ScriptedHop::Body(DATA.to_vec()),
            ScriptedHop::Body(DATA.to_vec()),
        ])
    }
    #[derive(Default)]
    struct FakeBuilder {
        called: Cell<bool>,
        fail: bool,
    }
    impl ConsentedRuntimeBuilder for FakeBuilder {
        fn build(&self, build: &RuntimeBuild<'_>) -> Result<(), ConsentedError> {
            self.called.set(true);
            // Inputs must be verified and in staging when tar/python would run.
            assert_eq!(
                fs::read(
                    build
                        .staging
                        .root
                        .join("inputs")
                        .join(build.entry.python.name)
                )
                .unwrap(),
                DATA
            );
            assert!(
                build
                    .staging
                    .root
                    .join("inputs/wheels")
                    .join(build.entry.wheels[0].name)
                    .exists()
            );
            if self.fail {
                return Err(ConsentedError::Build("fake python failure".into()));
            }
            for path in [&build.staging.python, &build.staging.venv] {
                super::super::ensure_private_dir(&path.join("bin")).unwrap();
            }
            fs::write(build.staging.python.join("bin/python3"), b"fake-python").unwrap();
            fs::write(build.staging.venv_python(), b"fake-venv-python").unwrap();
            fs::set_permissions(
                build.staging.venv_python(),
                fs::Permissions::from_mode(0o700),
            )
            .unwrap();
            fs::create_dir_all(
                build
                    .staging
                    .venv
                    .join("lib/python3.11/site-packages/fixture"),
            )
            .unwrap();
            fs::write(
                build
                    .staging
                    .venv
                    .join("lib/python3.11/site-packages/fixture/__init__.py"),
                b"fixture",
            )
            .unwrap();
            symlink("lib", build.staging.venv.join("lib64")).unwrap();
            Ok(())
        }
    }
    struct FakeHealth {
        fail: bool,
        called: Cell<bool>,
        expected_root: PathBuf,
    }
    impl ConsentedHealth for FakeHealth {
        fn check(&self, layout: &ConsentedLayout) -> Result<(), ConsentedError> {
            self.called.set(true);
            assert!(layout.root.exists());
            assert!(!layout.root.join("inputs").exists());
            assert_eq!(layout.root, self.expected_root);
            if self.fail {
                Err(ConsentedError::Health("fake CUDA health failure".into()))
            } else {
                Ok(())
            }
        }
    }
    fn health(store: &ModelStore, entry: &ConsentedCatalogEntry, fail: bool) -> FakeHealth {
        FakeHealth {
            fail,
            called: Cell::new(false),
            expected_root: install_root(store, entry.id, &content_hash(entry)),
        }
    }
    fn request<'a>(
        store: &'a ModelStore,
        fetcher: &'a dyn ArtifactFetcher,
        builder: &'a dyn ConsentedRuntimeBuilder,
        health: &'a dyn ConsentedHealth,
    ) -> ConsentedInstallRequest<'a> {
        ConsentedInstallRequest {
            store,
            fetcher,
            builder,
            health,
            consent: consent(),
            maintenance: &IdleMaintenance,
            io: InstallIo::default(),
            clock: InstallClock::default(),
            progress: &SilentProgress,
        }
    }
    pub(crate) fn install(store: &ModelStore) -> ConsentedReceipt {
        install_with_entry(
            request(
                store,
                &fetcher(),
                &FakeBuilder::default(),
                &health(store, &entry(), false),
            ),
            &entry(),
        )
        .unwrap()
    }
    pub(crate) fn replace_fixture_receipt(store: &ModelStore) -> ConsentedReceipt {
        let mut receipt = load_receipt(store).unwrap();
        receipt.consent.unix_time += 1;
        receipt.receipt_hash = receipt_hash(&receipt).unwrap();
        store_receipt(store, &receipt).unwrap();
        receipt
    }

    #[test]
    fn consented_install_verifies_and_removes_without_production_receipt() {
        let (_temp, store) = store();
        let receipt = install(&store);
        assert!(store.load_active().unwrap().is_none());
        let verified = verify_with_entry(&store, &entry()).unwrap();
        assert_eq!(verified.receipt(), &receipt);
        assert_eq!(receipt.device, "cuda:0");
        assert_eq!(receipt.compute_type, "int8_float16");
        assert_eq!(receipt.runtime_inputs.len(), 2);
        assert_eq!(receipt.files.len(), 12); // weight + fixture + all ten worker files
        assert!(!verified.layout.root.join("inputs").exists());
        assert!(matches!(
            remove_with_entry(&store, &entry()),
            Err(ConsentedError::Leased)
        ));
        assert!(receipt_path(&store).exists());
        let root = verified.layout.root.clone();
        drop(verified);
        remove_with_entry(&store, &entry()).unwrap();
        assert!(!root.exists());
        assert!(matches!(
            verify_with_entry(&store, &entry()),
            Err(ConsentedError::Missing)
        ));
    }
    #[test]
    fn consented_install_reports_monotonic_download_bytes_then_phases() {
        #[derive(Default)]
        struct Recorder(std::cell::RefCell<Vec<String>>);
        impl InstallProgress for Recorder {
            fn downloaded(&self, bytes: u64) {
                self.0.borrow_mut().push(format!("bytes {bytes}"));
            }
            fn phase(&self, phase: InstallPhase) {
                self.0.borrow_mut().push(format!("{phase:?}"));
            }
        }
        let (_temp, store) = store();
        let recorder = Recorder::default();
        let (fetcher, builder, health) = (
            fetcher(),
            FakeBuilder::default(),
            health(&store, &entry(), false),
        );
        let mut install = request(&store, &fetcher, &builder, &health);
        install.progress = &recorder;
        install_with_entry(install, &entry()).unwrap();
        let events = recorder.0.into_inner();
        let phases: Vec<_> = events.iter().filter(|e| !e.starts_with("bytes ")).collect();
        assert_eq!(phases, ["BuildingRuntime", "CheckingGpu"]);
        // Every byte arrives before the build, and the running total only grows
        // until it equals the sum of the pinned downloads.
        let fixture = entry();
        let pinned: u64 = fixture.weights.iter().map(|pin| pin.bytes).sum::<u64>()
            + (fixture.health_clip)().bytes
            + fixture.python.bytes
            + fixture.wheels.iter().map(|pin| pin.bytes).sum::<u64>();
        let build_at = events.iter().position(|e| e == "BuildingRuntime").unwrap();
        let mut total = 0_u64;
        for event in &events[..build_at] {
            let bytes: u64 = event.strip_prefix("bytes ").unwrap().parse().unwrap();
            assert!(bytes > 0);
            total += bytes;
        }
        assert_eq!(total, pinned);
    }
    #[test]
    fn consented_removal_accepts_old_worker_pins_and_missing_tree() {
        for missing_tree in [false, true] {
            let (_temp, store) = store();
            let mut receipt = install(&store);
            let root = install_root(&store, &receipt.entry_id, &receipt.content_hash);
            if missing_tree {
                fs::remove_dir_all(&root).unwrap();
            } else {
                // An update embeds different worker bytes; admission may refuse
                // the previous receipt, but explicit uninstall must remain possible.
                receipt.files.get_mut("worker/worker.py").unwrap().sha256 = "1".repeat(64);
                receipt.receipt_hash = receipt_hash(&receipt).unwrap();
                store_receipt(&store, &receipt).unwrap();
            }
            remove_with_entry(&store, &entry()).unwrap();
            assert!(!receipt_path(&store).exists());
            assert!(!root.exists());
        }
    }
    #[test]
    fn consented_removal_reads_manifestless_v1_receipt() {
        let (_temp, store) = store();
        let mut receipt = install(&store);
        let root = install_root(&store, &receipt.entry_id, &receipt.content_hash);
        receipt.version = 1;
        receipt.runtime_manifest.clear();
        receipt.receipt_hash = receipt_hash(&receipt).unwrap();
        let bytes = serde_json::to_vec(&receipt).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("runtime_manifest"));
        write_record(&store, "consented.receipt.json", &bytes).unwrap();
        assert!(matches!(
            verify_with_entry(&store, &entry()),
            Err(ConsentedError::Receipt)
        ));
        remove_with_entry(&store, &entry()).unwrap();
        assert!(!root.exists());
    }
    #[test]
    fn consented_gate_rejects_wrong_text_spdx_or_noninstallable_before_fetch() {
        let (_temp, store) = store();
        for kind in 0..3 {
            let builder = FakeBuilder::default();
            let fetcher = ScriptedFetcher::default();
            let health = health(&store, &entry(), false);
            let mut request = request(&store, &fetcher, &builder, &health);
            let mut entry = entry();
            match kind {
                0 => request.consent.text.push(' '),
                1 => request.consent.license_spdx = "Apache-2.0".into(),
                _ => entry.consent_installable = false,
            }
            assert!(matches!(
                install_with_entry(request, &entry),
                Err(ConsentedError::Consent)
            ));
            assert!(!builder.called.get());
            assert!(!receipt_path(&store).exists());
        }
    }
    #[test]
    fn consented_space_maintenance_and_lock_gates_precede_fetch() {
        let (_temp, store) = store();
        let builder = FakeBuilder::default();
        let fetcher = ScriptedFetcher::default();
        let health = health(&store, &entry(), false);
        let mut req = request(&store, &fetcher, &builder, &health);
        req.io.available_bytes = 1;
        assert!(matches!(
            install_with_entry(req, &entry()),
            Err(ConsentedError::NoSpace)
        ));
        let busy = BusyMaintenance {
            kind: MaintenanceKind::Recording,
        };
        let mut req = request(&store, &fetcher, &builder, &health);
        req.maintenance = &busy;
        assert!(matches!(
            install_with_entry(req, &entry()),
            Err(ConsentedError::Maintenance)
        ));
        let _lock = store.lock_exclusive().unwrap();
        assert!(matches!(
            install_with_entry(request(&store, &fetcher, &builder, &health), &entry()),
            Err(ConsentedError::Store(StoreError::Busy))
        ));
        assert!(!builder.called.get());
    }
    #[test]
    fn consented_each_input_digest_is_checked_before_builder() {
        let (_temp, store) = store();
        for bad in 0..4 {
            let mut bodies = [
                DATA.to_vec(),
                CatalogEntry::fixture_pcm_bytes().to_vec(),
                DATA.to_vec(),
                DATA.to_vec(),
            ];
            bodies[bad][0] ^= 1;
            let fetcher = ScriptedFetcher::new(bodies.into_iter().map(ScriptedHop::Body).collect());
            let builder = FakeBuilder::default();
            let health = health(&store, &entry(), false);
            assert!(matches!(
                install_with_entry(request(&store, &fetcher, &builder, &health), &entry()),
                Err(ConsentedError::Digest(_))
            ));
            assert!(!builder.called.get());
            assert!(!receipt_path(&store).exists());
            assert!(!install_root(&store, entry().id, &content_hash(&entry())).exists());
            assert_eq!(
                fs::read_dir(store.staging_dir("consented"))
                    .unwrap()
                    .count(),
                0
            );
        }
    }
    #[test]
    fn consented_streamed_failures_remove_partial_file_before_publication() {
        struct StreamingOnly {
            final_path: PathBuf,
            error: Option<FetchError>,
        }
        impl ArtifactFetcher for StreamingOnly {
            fn fetch(&self, _: &FetchRequest) -> Result<super::super::FetchResponse, FetchError> {
                panic!("consented download must use fetch_to");
            }
            fn fetch_to(
                &self,
                request: &FetchRequest,
                sink: &mut dyn Write,
            ) -> Result<super::super::FetchedSummary, FetchError> {
                assert!(!self.final_path.exists());
                let mut body = DATA.to_vec();
                body[0] ^= 1;
                sink.write_all(&body).map_err(super::super::fetch::io_err)?;
                assert!(!self.final_path.exists());
                if let Some(error) = &self.error {
                    return Err(error.clone());
                }
                Ok(super::super::FetchedSummary {
                    bytes: request.expected_bytes,
                    sha256_hex: sha256_hex(&body),
                    hops: 0,
                })
            }
        }
        let (_temp, store) = store();
        let dir = store.staging_dir("stream-test");
        super::super::ensure_private_dir(&dir).unwrap();
        let builder = FakeBuilder::default();
        let health = health(&store, &entry(), false);
        for error in [
            None,
            Some(FetchError::Truncated {
                expected: DATA.len() as u64,
                actual: 1,
            }),
            Some(FetchError::Oversized {
                expected: DATA.len() as u64,
                actual: DATA.len() as u64 + 1,
            }),
            Some(FetchError::NoSpace),
        ] {
            let fetcher = StreamingOnly {
                final_path: dir.join("model.bin"),
                error: error.clone(),
            };
            let result = download(
                &request(&store, &fetcher, &builder, &health),
                &WEIGHTS[0],
                &dir,
            )
            .unwrap_err();
            match error {
                None => {
                    assert!(matches!(result, ConsentedError::Digest(name) if name == "model.bin"))
                }
                Some(FetchError::NoSpace) => assert!(matches!(result, ConsentedError::NoSpace)),
                Some(error) => {
                    assert!(matches!(result, ConsentedError::Fetch(actual) if actual == error))
                }
            }
            assert!(!dir.join("model.bin").exists());
            assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
            assert!(!receipt_path(&store).exists());
        }
        let fetcher = ScriptedFetcher::new(vec![ScriptedHop::Body(DATA.to_vec())]);
        download(
            &request(&store, &fetcher, &builder, &health),
            &WEIGHTS[0],
            &dir,
        )
        .unwrap();
        assert_eq!(fs::read(dir.join("model.bin")).unwrap(), DATA);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        assert_eq!(
            fs::metadata(dir.join("model.bin"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn consented_redirect_escape_truncation_and_enospc_keep_receipt() {
        let (_temp, store) = store();
        let prior = install(&store);
        for hop in [
            ScriptedHop::Redirect("https://evil.test/python.tar.gz".into()),
            ScriptedHop::Truncated(b"short".to_vec()),
            ScriptedHop::NoSpace,
        ] {
            let fetcher = ScriptedFetcher::new(vec![hop]);
            assert!(
                install_with_entry(
                    request(
                        &store,
                        &fetcher,
                        &FakeBuilder::default(),
                        &health(&store, &entry(), false)
                    ),
                    &entry()
                )
                .is_err()
            );
            assert_eq!(load_receipt(&store).unwrap(), prior);
        }
        let fetcher = fetcher();
        let builder = FakeBuilder::default();
        let health = health(&store, &entry(), false);
        let mut req = request(&store, &fetcher, &builder, &health);
        req.io.fail_write = Some(libc::ENOSPC);
        assert!(matches!(
            install_with_entry(req, &entry()),
            Err(ConsentedError::NoSpace)
        ));
        assert_eq!(load_receipt(&store).unwrap(), prior);
    }
    #[test]
    fn consented_unreaped_health_retains_unreceipted_publish_and_next_install_resumes() {
        struct Unreaped;
        impl ConsentedHealth for Unreaped {
            fn check(&self, _: &ConsentedLayout) -> Result<(), ConsentedError> {
                Err(ConsentedError::HealthUnreaped)
            }
        }
        let (_temp, store) = store();
        let root = install_root(&store, entry().id, &content_hash(&entry()));
        assert!(matches!(
            install_with_entry(
                request(&store, &fetcher(), &FakeBuilder::default(), &Unreaped),
                &entry()
            ),
            Err(ConsentedError::HealthUnreaped)
        ));
        assert!(root.exists());
        assert!(!receipt_path(&store).exists());
        // Once the unit has stopped, a new install safely rebuilds this orphan.
        install(&store);
        assert!(verify_with_entry(&store, &entry()).is_ok());
    }

    #[test]
    fn consented_rollback_and_remove_preserve_a_live_health_lease() {
        use std::cell::RefCell;
        struct LeasedHealth(RefCell<Option<ConsentedLease>>);
        impl ConsentedHealth for LeasedHealth {
            fn check(&self, layout: &ConsentedLayout) -> Result<(), ConsentedError> {
                *self.0.borrow_mut() = Some(consented_install_lease(layout)?);
                Err(ConsentedError::Health(
                    "ordinary failure while still leased".into(),
                ))
            }
        }
        let (_temp, store) = store();
        let root = install_root(&store, entry().id, &content_hash(&entry()));
        let health = LeasedHealth(RefCell::new(None));
        assert!(
            install_with_entry(
                request(&store, &fetcher(), &FakeBuilder::default(), &health),
                &entry()
            )
            .is_err()
        );
        assert!(root.exists());
        assert!(!receipt_path(&store).exists());
        assert!(matches!(
            remove_with_entry(&store, &entry()),
            Err(ConsentedError::Leased)
        ));
        drop(health);
        remove_with_entry(&store, &entry()).unwrap();
        assert!(!root.exists());
    }

    #[test]
    fn consented_build_and_health_failure_remove_new_publish_keep_prior_receipt() {
        let (_temp, store) = store();
        let prior = install(&store);
        let mut next = entry();
        next.revision = "2";
        for fail_build in [true, false] {
            let builder = FakeBuilder {
                fail: fail_build,
                ..FakeBuilder::default()
            };
            assert!(
                install_with_entry(
                    request(
                        &store,
                        &fetcher(),
                        &builder,
                        &health(&store, &next, !fail_build)
                    ),
                    &next
                )
                .is_err()
            );
            assert_eq!(load_receipt(&store).unwrap(), prior);
            assert!(!install_root(&store, next.id, &content_hash(&next)).exists());
            assert!(verify_with_entry(&store, &entry()).is_ok());
        }
        assert!(
            install_with_entry(
                request(
                    &store,
                    &fetcher(),
                    &FakeBuilder::default(),
                    &health(&store, &entry(), true)
                ),
                &entry()
            )
            .is_err()
        );
        assert!(verify_with_entry(&store, &entry()).is_ok()); // reused prior publish remains
    }
    #[test]
    fn consented_aborts_at_each_step_leave_prior_receipt_and_no_new_publish() {
        let (_temp, store) = store();
        let prior = install(&store);
        let mut next = entry();
        next.revision = "2";
        for point in [
            InstallAbort::AfterLock,
            InstallAbort::AfterStaging,
            InstallAbort::AfterFetch,
            InstallAbort::AfterPublish,
            InstallAbort::AfterHealth,
        ] {
            let fetcher = fetcher();
            let builder = FakeBuilder::default();
            let health = health(&store, &next, false);
            let mut req = request(&store, &fetcher, &builder, &health);
            req.io.abort = Some(point);
            assert!(matches!(
                install_with_entry(req, &next),
                Err(ConsentedError::Aborted)
            ));
            assert_eq!(load_receipt(&store).unwrap(), prior);
            assert!(!install_root(&store, next.id, &content_hash(&next)).exists());
        }
    }
    #[test]
    fn consented_install_deadline_prevents_fetch() {
        let (_temp, store) = store();
        let fetcher = ScriptedFetcher::default();
        let builder = FakeBuilder::default();
        let health = health(&store, &entry(), false);
        let req = request(&store, &fetcher, &builder, &health);
        req.clock.advance(Duration::from_secs(1801));
        assert!(matches!(
            install_with_entry(req, &entry()),
            Err(ConsentedError::Deadline)
        ));
        assert!(!builder.called.get());
    }
    #[test]
    fn consented_verification_rehashes_weights_fixture_and_worker() {
        for name in [
            "weights/model.bin",
            "jfk.wav",
            "worker/worker.py",
            "worker/voisu_asr/candidates.py",
        ] {
            let (_temp, store) = store();
            let receipt = install(&store);
            let path = install_root(&store, &receipt.entry_id, &receipt.content_hash).join(name);
            let mut bytes = fs::read(&path).unwrap();
            bytes[0] ^= 1;
            fs::write(path, bytes).unwrap();
            assert!(matches!(
                verify_with_entry(&store, &entry()),
                Err(ConsentedError::Digest(_))
            ));
        }
    }
    #[test]
    fn consented_receipt_cannot_change_paths_runtime_inputs_or_pins() {
        let (_temp, store) = store();
        let prior = install(&store);
        for kind in 0..5 {
            let mut changed = prior.clone();
            match kind {
                0 => changed.venv_path = "/usr/bin/python3".into(),
                1 => changed.files.clear(),
                2 => changed.runtime_inputs.clear(),
                3 => changed.consent.text.push('!'),
                _ => changed.device = "cpu".into(),
            }
            changed.receipt_hash = receipt_hash(&changed).unwrap(); // attacker recomputes unkeyed hash
            store_receipt(&store, &changed).unwrap();
            assert!(matches!(
                verify_with_entry(&store, &entry()),
                Err(ConsentedError::Receipt)
            ));
        }
    }
    #[test]
    fn consented_verifier_refuses_runtime_write_modes_and_symlink_python() {
        let (_temp, store) = store();
        let receipt = install(&store);
        let root = install_root(&store, &receipt.entry_id, &receipt.content_hash);
        let python = root.join("runtime/venv/bin/python");
        fs::set_permissions(&python, fs::Permissions::from_mode(0o720)).unwrap();
        assert!(verify_with_entry(&store, &entry()).is_err());
        fs::set_permissions(&python, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(verify_with_entry(&store, &entry()).is_err());
        fs::remove_file(&python).unwrap();
        symlink("../../python/bin/python3", &python).unwrap();
        assert!(verify_with_entry(&store, &entry()).is_err());
    }
    #[test]
    fn consented_receipt_fsync_failure_restores_prior_or_retains_both_trees() {
        for rollback_fails in [false, true] {
            let (_temp, store) = store();
            let prior = install(&store);
            let prior_root = install_root(&store, prior.entry_id.as_str(), &prior.content_hash);
            let mut next = entry();
            next.revision = "2";
            let new_root = install_root(&store, next.id, &content_hash(&next));
            let calls = Cell::new(0);
            let result = install_with_receipt_sync(
                request(
                    &store,
                    &fetcher(),
                    &FakeBuilder::default(),
                    &health(&store, &next, false),
                ),
                &next,
                |parent| {
                    let call = calls.get();
                    calls.set(call + 1);
                    if call == 0 {
                        // Injection occurs after the real replacement: this is
                        // the new receipt and exact final publication path.
                        let active = load_receipt(&store).unwrap();
                        assert_eq!(active.revision, "2");
                        assert_eq!(
                            install_root(&store, &active.entry_id, &active.content_hash),
                            new_root
                        );
                        assert!(new_root.exists());
                        return Err(std::io::Error::from_raw_os_error(libc::EIO));
                    }
                    if rollback_fails {
                        Err(std::io::Error::from_raw_os_error(libc::EIO))
                    } else {
                        sync_dir(parent)
                    }
                },
            );
            assert!(
                matches!(result, Err(ConsentedError::Io(error)) if error.raw_os_error() == Some(libc::EIO))
            );
            assert_eq!(calls.get(), 2);
            assert_eq!(load_receipt(&store).unwrap(), prior);
            assert!(prior_root.exists());
            // With uncertain rollback fsync, either namespace version could
            // survive power loss, so both possible receipt targets stay alive.
            assert_eq!(new_root.exists(), rollback_fails);
            if rollback_fails {
                let _lock = store.lock_exclusive().unwrap();
                assert!(matches!(
                    remove_unreceipted_publish_with_sync(&store, &next, |parent| {
                        assert_eq!(parent, store.root().join("private"));
                        Err(std::io::Error::from_raw_os_error(libc::EIO))
                    }),
                    Err(ConsentedError::Io(error)) if error.raw_os_error() == Some(libc::EIO)
                ));
                assert!(new_root.exists());
                assert!(prior_root.exists());
                assert_eq!(load_receipt(&store).unwrap(), prior);
                assert!(remove_unreceipted_publish(&store, &next).unwrap());
                assert!(!new_root.exists());
                assert!(prior_root.exists());
                assert_eq!(load_receipt(&store).unwrap(), prior);
            }
            assert!(verify_with_entry(&store, &entry()).is_ok());
        }
    }
    #[test]
    fn consented_uncertain_receipt_unlink_retains_publish_until_cleanup_syncs() {
        let (_temp, store) = store();
        let root = install_root(&store, entry().id, &content_hash(&entry()));
        let calls = Cell::new(0);
        assert!(matches!(
            install_with_receipt_sync(
                request(
                    &store,
                    &fetcher(),
                    &FakeBuilder::default(),
                    &health(&store, &entry(), false),
                ),
                &entry(),
                |_| {
                    calls.set(calls.get() + 1);
                    Err(std::io::Error::from_raw_os_error(libc::EIO))
                },
            ),
            Err(ConsentedError::Io(error)) if error.raw_os_error() == Some(libc::EIO)
        ));
        assert_eq!(calls.get(), 2);
        assert!(matches!(load_receipt(&store), Err(ConsentedError::Missing)));
        assert!(root.exists());

        let _lock = store.lock_exclusive().unwrap();
        assert!(matches!(
            remove_unreceipted_publish_with_sync(&store, &entry(), |parent| {
                assert_eq!(parent, store.root().join("private"));
                Err(std::io::Error::from_raw_os_error(libc::EIO))
            }),
            Err(ConsentedError::Io(error)) if error.raw_os_error() == Some(libc::EIO)
        ));
        assert!(root.exists());
        assert!(remove_unreceipted_publish(&store, &entry()).unwrap());
        assert!(!root.exists());
    }

    #[test]
    fn consented_runtime_manifest_detects_truncation_deletion_links_and_extras() {
        for mutation in 0..5 {
            let (_temp, store) = store();
            let receipt = install(&store);
            let root = install_root(&store, &receipt.entry_id, &receipt.content_hash);
            let runtime = root.join("runtime");
            match mutation {
                0 => fs::write(runtime.join("venv/bin/python"), b"short").unwrap(),
                1 => fs::remove_dir_all(runtime.join("venv/lib/python3.11/site-packages/fixture"))
                    .unwrap(),
                2 => {
                    fs::remove_file(runtime.join("venv/lib64")).unwrap();
                    symlink("../python", runtime.join("venv/lib64")).unwrap();
                }
                3 => fs::write(runtime.join("venv/bin/unexpected"), b"extra").unwrap(),
                _ => fs::write(
                    runtime.join("venv/lib/python3.11/site-packages/extra.pth"),
                    b"extra",
                )
                .unwrap(),
            }
            assert!(
                verify_with_entry(&store, &entry()).is_err(),
                "mutation {mutation}"
            );
        }
    }
    #[test]
    fn consented_full_verifier_detects_same_size_runtime_edits() {
        let (_temp, store) = store();
        let receipt = install(&store);
        let root = install_root(&store, &receipt.entry_id, &receipt.content_hash);
        let path = root.join("runtime/venv/lib/python3.11/site-packages/fixture/__init__.py");
        fs::write(&path, b"changed").unwrap(); // same length as "fixture"
        assert!(verify_with_entry(&store, &entry()).is_ok());
        assert!(verify_with_entry_mode(&store, &entry(), true, || {}).is_err());
    }
    #[test]
    fn consented_launch_allows_audited_deep_cache_full_verifier_rejects_extras() {
        let (_temp, store) = store();
        let receipt = install(&store);
        let root = install_root(&store, &receipt.entry_id, &receipt.content_hash);
        let cache = root.join("runtime/venv/lib/python3.11/site-packages/fixture/__pycache__");
        fs::create_dir(&cache).unwrap();
        fs::write(cache.join("fixture.pyc"), b"cache").unwrap();
        let install = verify_with_entry(&store, &entry()).unwrap();
        let observation = ConsentedObservation::after_verification(&install);
        drop(install);
        assert!(observation.unchanged());
        assert!(verify_with_entry_mode(&store, &entry(), true, || {}).is_err());
        fs::set_permissions(cache.join("fixture.pyc"), fs::Permissions::from_mode(0o666)).unwrap();
        assert!(!observation.unchanged());
        assert!(verify_with_entry(&store, &entry()).is_err());
    }
    #[test]
    fn consented_runtime_manifest_is_covered_by_receipt_hash() {
        let (_temp, store) = store();
        let mut receipt = install(&store);
        receipt.runtime_manifest.remove("venv/lib64");
        // Leave the hash intact; metadata-only admission must reject this.
        write_record(
            &store,
            "consented.receipt.json",
            &serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            verify_with_entry(&store, &entry()),
            Err(ConsentedError::Receipt)
        ));
    }
    #[test]
    fn consented_tombstone_resumes_after_receipt_deletion_on_all_entrypoints() {
        for retry in 0..3 {
            let (_temp, store) = store();
            let receipt = install(&store);
            let root = install_root(&store, &receipt.entry_id, &receipt.content_hash);
            assert!(matches!(
                remove_with_delete_hook(&store, &entry(), || Err(ConsentedError::Aborted)),
                Err(ConsentedError::Aborted)
            ));
            assert!(!receipt_path(&store).exists());
            assert!(root.exists());
            assert!(store.root().join("private/consented.removal.json").exists());
            let lease = lease_file(&store, &receipt.content_hash).unwrap();
            flock(&lease, libc::LOCK_SH).unwrap();
            assert!(matches!(
                remove_with_entry(&store, &entry()),
                Err(ConsentedError::Leased)
            ));
            assert!(root.exists());
            drop(lease);
            match retry {
                0 => assert_eq!(
                    remove_with_entry(&store, &entry()).unwrap(),
                    ConsentedRemoveOutcome::Removed
                ),
                1 => assert!(matches!(
                    verify_with_entry(&store, &entry()),
                    Err(ConsentedError::Missing)
                )),
                _ => {
                    let mut next = entry();
                    next.revision = "2";
                    install_with_entry(
                        request(
                            &store,
                            &fetcher(),
                            &FakeBuilder::default(),
                            &health(&store, &next, false),
                        ),
                        &next,
                    )
                    .unwrap();
                }
            }
            assert!(!root.exists());
            assert!(!store.root().join("private/consented.removal.json").exists());
        }
    }
    #[test]
    fn consented_real_venv_runs_after_install_publication() {
        use super::super::consented_runtime::{
            SystemConsentedCommandRunner, prepared_run, relocate_venv,
        };
        use std::process::Command;
        match Command::new("python3")
            .args(["-I", "-c", "import venv"])
            .output()
        {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                eprintln!(
                    "SKIP consented_real_venv_runs_after_install_publication: no host python3"
                );
                return;
            }
            result => assert!(
                result.unwrap().status.success(),
                "host python3 cannot import venv"
            ),
        }
        struct HostVenv;
        impl ConsentedRuntimeBuilder for HostVenv {
            fn build(&self, build: &RuntimeBuild<'_>) -> Result<(), ConsentedError> {
                FakeBuilder::default().build(build)?;
                fs::remove_dir_all(&build.staging.venv)?;
                prepared_run(
                    &SystemConsentedCommandRunner,
                    Command::new("python3")
                        .args(["-I", "-m", "venv", "--copies", "--without-pip"])
                        .arg(&build.staging.venv),
                    &build.staging.root,
                    build.clock,
                )?;
                relocate_venv(build)
            }
        }
        struct PrefixHealth {
            expected: PathBuf,
        }
        impl ConsentedHealth for PrefixHealth {
            fn check(&self, layout: &ConsentedLayout) -> Result<(), ConsentedError> {
                assert_eq!(layout.root, self.expected);
                assert!(!layout.root.join("inputs").exists());
                let output = Command::new(layout.venv_python())
                    .env_clear()
                    .args(["-I", "-c", "import sys; print(sys.prefix)"])
                    .output()?;
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert_eq!(
                    String::from_utf8(output.stdout).unwrap().trim(),
                    layout.venv.to_str().unwrap()
                );
                Ok(())
            }
        }
        let (_temp, store) = store();
        let expected = install_root(&store, entry().id, &content_hash(&entry()));
        let health = PrefixHealth { expected };
        install_with_entry(request(&store, &fetcher(), &HostVenv, &health), &entry()).unwrap();
        assert!(verify_with_entry(&store, &entry()).is_ok());
    }
    #[test]
    fn consented_catalog_pins_are_complete_and_group_scoped() {
        assert_eq!(CONSENTED_ENTRY.weights.len(), 5);
        assert_eq!(CONSENTED_ENTRY.wheels.len(), 26);
        assert_eq!(CONSENTED_ENTRY.total_download_bytes, 2_980_914_661);
        let sum = CONSENTED_ENTRY
            .weights
            .iter()
            .chain(std::iter::once(&CONSENTED_ENTRY.python))
            .chain(CONSENTED_ENTRY.wheels)
            .map(|pin| pin.bytes)
            .sum::<u64>()
            + (CONSENTED_ENTRY.health_clip)().bytes;
        assert_eq!(sum, CONSENTED_ENTRY.total_download_bytes);
        for pin in CONSENTED_ENTRY
            .weights
            .iter()
            .chain(std::iter::once(&CONSENTED_ENTRY.python))
            .chain(CONSENTED_ENTRY.wheels)
        {
            super::super::validate_catalog_url(pin.url, pin.hosts).unwrap();
            assert_eq!(pin.sha256.len(), 64);
        }
        assert_eq!(
            CONSENTED_ENTRY.weights[0].hosts,
            &["huggingface.co", "us.aws.cdn.hf.co"]
        );
        assert_eq!(
            CONSENTED_ENTRY.python.hosts,
            &["github.com", "release-assets.githubusercontent.com"]
        );
        assert!(
            CONSENTED_ENTRY
                .wheels
                .iter()
                .all(|pin| pin.hosts == ["files.pythonhosted.org"])
        );
        assert!(super::super::production_selection(&super::super::shipped_catalog()).is_none());
    }
}
