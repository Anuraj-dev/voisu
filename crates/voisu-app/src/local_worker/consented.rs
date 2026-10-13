//! Offline worker for an explicitly consented install.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use sha2::{Digest, Sha256};

use super::bounds::{
    CANCEL_GRACE, LOAD_DEADLINE, WorkerResourceLimits, resolve_live_worker_limits,
};
use super::dev::DevWorker;
use super::dev::GPU_BUSY;
use super::protocol::{ControlFrame, Correlation, WorkerFrame};
use super::supervisor::{ReapOutcome, SupervisorError, WorkerChild};
use crate::local_model::{ActiveReceipt, FilePin};
use crate::local_model::{
    CONSENTED_ENTRY, ConsentedCatalogEntry, ConsentedError, ConsentedFilePin, ConsentedHealth,
    ConsentedLayout, ConsentedReceipt, ConsentedRuntimeEntry, DevFilePin, DevLimits, DevManifest,
    DevRuntimeManifest, EXPECTED_HEALTH_TEXT, ModelStore, VerifiedConsentedInstall,
    consented_worker_files, development_catalog_entry, ensure_private_dir, models_dir_from_state,
    sha256_hex, verify_consented_install,
};

/// Installation's real, isolated health probe. `state_dir` is the Voisu
/// state directory (the parent of `models`), never the install tree.
pub struct ConsentedWorkerHealth {
    state_dir: PathBuf,
    entry: &'static ConsentedCatalogEntry,
}

impl ConsentedWorkerHealth {
    pub fn new(state_dir: PathBuf) -> Self {
        Self::for_entry(state_dir, &CONSENTED_ENTRY)
    }

    pub fn for_entry(state_dir: PathBuf, entry: &'static ConsentedCatalogEntry) -> Self {
        Self { state_dir, entry }
    }
}

impl ConsentedHealth for ConsentedWorkerHealth {
    fn check(
        &self,
        layout: &ConsentedLayout,
        receipt: &ConsentedReceipt,
    ) -> Result<(), ConsentedError> {
        // The installer holds its exclusive store lock and has verified the
        // published tree; the receipt is committed only after this probe.
        let lease = crate::local_model::consented::consented_install_lease(layout)?;
        let pins = catalog_pins(self.entry);
        let (manifest, hash, file) = prepare_manifest(
            layout,
            &pins,
            &self.state_dir,
            self.entry,
            &receipt.runtime_manifest,
        )?;
        let mut worker =
            DevWorker::launch_consented(manifest, hash.clone(), file, &layout.root, None, lease)
                .map_err(health_error)?;
        check_worker_health_on_device(&mut worker, &hash, self.entry.device)
    }
}

fn health_error(error: SupervisorError) -> ConsentedError {
    match error {
        SupervisorError::Busy | SupervisorError::Unavailable(GPU_BUSY) => ConsentedError::GpuBusy,
        other => ConsentedError::Health(other.to_string()),
    }
}

#[cfg(test)]
fn check_worker_health(worker: &mut impl WorkerChild, hash: &str) -> Result<(), ConsentedError> {
    check_worker_health_on_device(worker, hash, "cuda:0")
}

fn check_worker_health_on_device(
    worker: &mut impl WorkerChild,
    hash: &str,
    device: &str,
) -> Result<(), ConsentedError> {
    let correlation = Correlation {
        daemon_nonce: "consented-health".into(),
        generation: 1,
        request_id: "consented-health-prepare".into(),
        recording_id: "health".into(),
        model_receipt_hash: hash.into(),
    };
    let result = (|| {
        let reply = worker
            .exchange(
                ControlFrame::Prepare(correlation.clone()),
                None,
                Instant::now() + LOAD_DEADLINE,
            )
            .map_err(health_error)?;
        match reply {
            WorkerFrame::Ready {
                correlation: observed,
                observed_device,
            } if observed == correlation && observed_device == device => Ok(()),
            WorkerFrame::Error { code, .. } => Err(ConsentedError::Health(code)),
            _ => Err(ConsentedError::Health(format!(
                "health did not prove ready on {device}"
            ))),
        }
    })();
    if result.is_ok() {
        // Shutdown is best effort; the unit's observed stop/reap is mandatory.
        let _ = worker.exchange(
            ControlFrame::Cancel {
                request_id: "consented-health-shutdown".into(),
            },
            None,
            Instant::now() + CANCEL_GRACE,
        );
    }
    let reaped = worker
        .cancel_and_reap()
        .map_err(|_| ConsentedError::HealthUnreaped)?;
    if reaped == ReapOutcome::Unreaped {
        return Err(ConsentedError::HealthUnreaped);
    }
    result
}

pub(crate) fn verify_default_consented_install() -> Result<VerifiedConsentedInstall, ConsentedError>
{
    let dir = models_dir_from_state(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
        .map_err(ConsentedError::Store)?;
    let store = ModelStore::open(dir).map_err(ConsentedError::Store)?;
    verify_consented_install(&store)
}

pub(crate) fn select_consented_receipt(
    pilot: bool,
    verify: impl FnOnce() -> Result<VerifiedConsentedInstall, ConsentedError>,
) -> Option<ActiveReceipt> {
    if pilot {
        return None;
    }
    verify()
        .ok()
        .map(|install| consented_active_receipt(&install))
}

pub(crate) fn consented_active_receipt(install: &VerifiedConsentedInstall) -> ActiveReceipt {
    let receipt = install.receipt();
    ActiveReceipt {
        catalog_id: receipt.entry_id.clone(),
        catalog_revision: receipt.revision.clone(),
        runtime_abi: format!("voisu-consented:{}@{}", receipt.revision, receipt.device),
        protocol: super::protocol::PROTOCOL_VERSION,
        language: "en".into(),
        artifact_id: receipt.content_hash.clone(),
        files: receipt
            .files
            .iter()
            .map(|(name, pin)| {
                (
                    name.clone(),
                    FilePin {
                        sha256_hex: pin.sha256.clone(),
                        bytes: pin.bytes,
                    },
                )
            })
            .collect(),
        receipt_hash: receipt.receipt_hash.clone(),
    }
}

#[cfg(not(test))]
pub(crate) fn launch_consented_worker(
    install: VerifiedConsentedInstall,
) -> Result<DevWorker, ConsentedError> {
    let layout = install.layout();
    let models = layout
        .root
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .ok_or(ConsentedError::Receipt)?;
    let state = models.parent().ok_or(ConsentedError::Receipt)?;
    let (manifest, hash, file) = prepare_manifest(
        layout,
        &install.receipt().files,
        state,
        crate::local_model::consented_entry(&install.receipt().entry_id)
            .ok_or(ConsentedError::Receipt)?,
        &install.receipt().runtime_manifest,
    )?;
    let root = layout.root.clone();
    let lease = crate::local_model::consented::consented_install_lease(install.layout())?;
    DevWorker::launch_consented(manifest, hash, file, &root, Some(install), lease)
        .map_err(health_error)
}

fn catalog_pins(entry: &ConsentedCatalogEntry) -> BTreeMap<String, ConsentedFilePin> {
    let mut pins = BTreeMap::new();
    for pin in entry.weights {
        pins.insert(
            format!("weights/{}", pin.name),
            ConsentedFilePin {
                sha256: pin.sha256.into(),
                bytes: pin.bytes,
            },
        );
    }
    let health = (entry.health_clip)();
    pins.insert(
        "jfk.wav".into(),
        ConsentedFilePin {
            sha256: health.sha256.into(),
            bytes: health.bytes,
        },
    );
    for (name, bytes) in consented_worker_files() {
        pins.insert(
            format!("worker/{name}"),
            ConsentedFilePin {
                sha256: sha256_hex(bytes),
                bytes: bytes.len() as u64,
            },
        );
    }
    pins
}

fn prepare_manifest(
    layout: &ConsentedLayout,
    pins: &BTreeMap<String, ConsentedFilePin>,
    state: &Path,
    entry: &ConsentedCatalogEntry,
    runtime_manifest: &BTreeMap<String, ConsentedRuntimeEntry>,
) -> Result<(DevManifest, String, tempfile::NamedTempFile), ConsentedError> {
    if !state.is_absolute() || layout.root.to_str().is_none() || state.to_str().is_none() {
        return Err(ConsentedError::Tree(
            "worker paths must be absolute UTF-8".into(),
        ));
    }
    let limits = resolve_live_worker_limits()
        .ok_or_else(|| ConsentedError::Health("cannot resolve worker RAM limit".into()))?
        .limits;
    let cache = state.join("consented-worker/cache");
    if cache.starts_with(&layout.root) {
        return Err(ConsentedError::Tree(
            "cache must be outside the read-only install".into(),
        ));
    }
    ensure_private_dir(&cache).map_err(ConsentedError::Fs)?;
    let python_path = layout.venv_python();
    let mut python = std::fs::File::open(&python_path)?;
    let mut digest = Sha256::new();
    let mut buf = [0_u8; 64 * 1024];
    loop {
        let size = python.read(&mut buf)?;
        if size == 0 {
            break;
        }
        digest.update(&buf[..size]);
    }
    let pin = DevFilePin {
        path: python_path.to_string_lossy().into_owned(),
        sha256: format!("{:x}", digest.finalize()),
        bytes: python.metadata()?.len(),
    };
    let mut pins = pins.clone();
    if entry.native_runtime.is_some() {
        let relative = crate::local_model::NEMO_LIBRARY;
        let key = relative
            .strip_prefix("runtime/")
            .ok_or(ConsentedError::Receipt)?;
        let pin = match runtime_manifest.get(key) {
            Some(ConsentedRuntimeEntry::File { sha256, bytes }) => ConsentedFilePin {
                sha256: sha256.clone(),
                bytes: *bytes,
            },
            _ => return Err(ConsentedError::Receipt),
        };
        pins.insert(relative.into(), pin);
    }
    let manifest = manifest_from_pins(layout, &pins, pin, limits, &cache, entry);
    manifest
        .validate()
        .map_err(|error| ConsentedError::Health(error.to_string()))?;
    let bytes = serde_json::to_vec(&manifest).map_err(|_| ConsentedError::Receipt)?;
    let hash = sha256_hex(&bytes);
    // tempfile creates a unique file with mode 0600. Keep it for the worker's
    // entire lifetime; it is outside the sole writable cache directory.
    let mut file = tempfile::Builder::new()
        .prefix("manifest-")
        .suffix(".json")
        .tempfile_in(state.join("consented-worker"))?;
    file.write_all(&bytes)?;
    file.as_file().sync_all()?;
    Ok((manifest, hash, file))
}

fn manifest_from_pins(
    layout: &ConsentedLayout,
    pins: &BTreeMap<String, ConsentedFilePin>,
    python: DevFilePin,
    limits: WorkerResourceLimits,
    cache: &Path,
    entry: &ConsentedCatalogEntry,
) -> DevManifest {
    let mut files = pins
        .iter()
        .map(|(name, pin)| DevFilePin {
            path: layout.root.join(name).to_string_lossy().into_owned(),
            sha256: pin.sha256.clone(),
            bytes: pin.bytes,
        })
        .collect::<Vec<_>>();
    files.push(python);
    DevManifest {
        version: 1,
        model_id: entry.id.into(),
        device: entry.device.into(),
        language: "en".into(),
        runtime: DevRuntimeManifest {
            python: layout.venv_python().to_string_lossy().into_owned(),
            worker: layout
                .worker
                .join("worker.py")
                .to_string_lossy()
                .into_owned(),
            revision: entry.revision.into(),
        },
        model_path: if entry.native_runtime.is_some() {
            layout
                .weights
                .join(entry.weights[0].name)
                .to_string_lossy()
                .into_owned()
        } else {
            layout.weights.to_string_lossy().into_owned()
        },
        files,
        options: {
            let mut options = serde_json::json!({
                "revision": entry.revision,
                "compute_type": entry.compute_type,
                "health_audio": layout.health_clip,
                "health_expected": EXPECTED_HEALTH_TEXT,
                "cache_dir": cache,
            })
            .as_object()
            .expect("object")
            .clone();
            if entry.native_runtime.is_some() {
                options.remove("revision");
                options.remove("compute_type");
                options.insert(
                    "library".into(),
                    serde_json::json!(layout.root.join(crate::local_model::NEMO_LIBRARY)),
                );
                options.insert("language_code".into(), serde_json::json!("en"));
            }
            options
        },
        limits: DevLimits {
            memory_bytes: limits.memory_max_bytes,
            tasks: u64::from(limits.tasks_max),
            cpu_cores: limits.cpu_quota_cores,
            cache_bytes: limits.cache_max_bytes,
            vram_bytes: if entry.device == "cpu" {
                0
            } else {
                development_catalog_entry(entry.id)
                    .expect("CUDA catalog entry")
                    .vram_bytes
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::super::bounds::resolve_worker_limits;
    use super::*;
    use serde_json::json;

    #[test]
    #[ignore = "downloads 734 MiB and runs the real CPU sandbox; use temporary XDG roots"]
    fn real_cpu_consented_install_offline_smoke() {
        use crate::local_model::{
            ConsentedConsent, ConsentedInstallRequest, InstallClock, InstallIo, MaintenanceError,
            MaintenanceKind, MaintenanceReservation, OfflineRuntimeBuilder, PARAKEET_ENTRY,
            ProductionHttps, SilentProgress,
        };
        struct Idle;
        impl MaintenanceReservation for Idle {
            fn acquire(&self) -> Result<MaintenanceKind, MaintenanceError> {
                Ok(MaintenanceKind::Idle)
            }
        }
        let base = PathBuf::from(
            std::env::var_os("XDG_STATE_HOME").expect("smoke needs temporary XDG_STATE_HOME"),
        );
        assert!(base.is_absolute());
        assert_eq!(std::env::var("CUDA_VISIBLE_DEVICES").as_deref(), Ok(""));
        ensure_private_dir(&base).unwrap();
        // Retain failed runs: an unreaped worker must never lose its tree.
        let state = tempfile::Builder::new()
            .prefix("cpu-smoke-")
            .tempdir_in(base)
            .unwrap()
            .keep();
        eprintln!("CPU SMOKE state={}", state.display());
        let store = ModelStore::open(state.as_path().join("models")).unwrap();
        let receipt = crate::local_model::install_consented_install(ConsentedInstallRequest {
            store: &store,
            consent: ConsentedConsent::accepted_entry(&PARAKEET_ENTRY, 1),
            fetcher: &ProductionHttps,
            builder: &OfflineRuntimeBuilder,
            health: &ConsentedWorkerHealth::for_entry(state.as_path().into(), &PARAKEET_ENTRY),
            maintenance: &Idle,
            io: InstallIo::default(),
            clock: InstallClock::default(),
            progress: &SilentProgress,
        })
        .expect("real pinned CPU install and isolated health");
        assert_eq!(receipt.entry_id, "parakeet-tdt-0.6b-v3");
        assert_eq!(receipt.device, "cpu");
        let extracted: u64 = receipt
            .runtime_manifest
            .values()
            .filter_map(|item| match item {
                crate::local_model::ConsentedRuntimeEntry::File { bytes, .. } => Some(bytes),
                _ => None,
            })
            .sum();
        eprintln!(
            "CPU SMOKE extracted_runtime_bytes={extracted} download_bytes={}",
            PARAKEET_ENTRY.total_download_bytes
        );
        assert!(extracted <= PARAKEET_ENTRY.estimated_extracted_runtime_bytes);
        assert_eq!(
            receipt.runtime_inputs[PARAKEET_ENTRY.native_runtime.unwrap().name].sha256,
            PARAKEET_ENTRY.native_runtime.unwrap().sha256
        );

        let install = crate::local_model::verify_consented_install_full(&store).unwrap();
        let layout = install.layout().clone();
        let (manifest, hash, file) = prepare_manifest(
            &layout,
            &receipt.files,
            state.as_path(),
            &PARAKEET_ENTRY,
            &receipt.runtime_manifest,
        )
        .unwrap();
        let pcm = std::process::Command::new(layout.venv_python()).env_clear()
            .args(["-I", "-c", "import sys,wave; w=wave.open(sys.argv[1]); sys.stdout.buffer.write(w.readframes(w.getnframes()))"])
            .arg(&layout.health_clip).output().unwrap();
        assert!(pcm.status.success());
        let lease = crate::local_model::consented::consented_install_lease(&layout).unwrap();
        let mut worker = DevWorker::launch_consented(
            manifest,
            hash.clone(),
            file,
            &layout.root,
            Some(install),
            lease,
        )
        .unwrap();
        let mut correlation = Correlation {
            daemon_nonce: "cpu-smoke".into(),
            generation: 1,
            request_id: "prepare".into(),
            recording_id: "jfk".into(),
            model_receipt_hash: hash,
        };
        let ready = worker
            .exchange(
                ControlFrame::Prepare(correlation.clone()),
                None,
                Instant::now() + LOAD_DEADLINE,
            )
            .unwrap();
        assert!(
            matches!(&ready, WorkerFrame::Ready { observed_device, .. } if observed_device == "cpu"),
            "{ready:?}"
        );
        correlation.request_id = "jfk-transcribe".into();
        let started = Instant::now();
        let reply = worker
            .exchange(
                ControlFrame::Transcribe {
                    correlation,
                    pcm_bytes: pcm.stdout.len(),
                },
                Some(&pcm.stdout),
                Instant::now() + super::super::bounds::STOP_PROCESSING,
            )
            .unwrap();
        eprintln!(
            "CPU SMOKE clip=jfk.wav ready={ready:?} latency={:?} reply={reply:?}",
            started.elapsed()
        );
        assert!(matches!(reply, WorkerFrame::Transcript { .. }));
        let metrics = worker.last_inference_metrics();
        eprintln!("CPU SMOKE metrics={metrics:?}");
        assert_eq!(metrics.peak_gpu_memory_bytes, Some(0));
        assert_ne!(worker.cancel_and_reap().unwrap(), ReapOutcome::Unreaped);
        drop(worker);
        assert_eq!(
            crate::local_model::remove_consented_install(&store).unwrap(),
            crate::local_model::consented::ConsentedRemoveOutcome::Removed
        );
        std::fs::remove_dir_all(state).unwrap();
    }

    #[test]
    fn cpu_manifest_pins_native_library_has_no_vram_and_uses_weight_file() {
        use crate::local_model::PARAKEET_ENTRY;
        let state = tempfile::tempdir().unwrap();
        let layout = ConsentedLayout::at(state.path().join("models/consented/parakeet/hash"));
        let pins = catalog_pins(&PARAKEET_ENTRY);
        for name in pins
            .keys()
            .map(String::as_str)
            .chain([crate::local_model::NEMO_LIBRARY])
        {
            let path = layout.root.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"fixture").unwrap();
        }
        let python = layout.venv_python();
        std::fs::create_dir_all(python.parent().unwrap()).unwrap();
        std::fs::write(python, b"python").unwrap();
        let runtime_manifest = native_library_runtime_manifest(b"fixture");
        let (manifest, _, _) = prepare_manifest(
            &layout,
            &pins,
            state.path(),
            &PARAKEET_ENTRY,
            &runtime_manifest,
        )
        .unwrap();
        assert_eq!(manifest.model_id, "parakeet-tdt-0.6b-v3");
        assert_eq!(manifest.device, "cpu");
        assert_eq!(manifest.limits.vram_bytes, 0);
        assert!(!manifest.options.contains_key("revision"));
        assert!(!manifest.options.contains_key("compute_type"));
        assert_eq!(
            manifest.model_path,
            layout
                .weights
                .join("parakeet-tdt-0.6b-v3.q8_0.gguf")
                .to_str()
                .unwrap()
        );
        let library = layout.root.join(crate::local_model::NEMO_LIBRARY);
        assert_eq!(manifest.options["library"], library.to_str().unwrap());
        assert!(manifest.files.iter().any(
            |pin| pin.path == library.to_str().unwrap() && pin.sha256 == sha256_hex(b"fixture")
        ));
        assert_eq!(
            development_catalog_entry(&manifest.model_id)
                .unwrap()
                .runtime,
            crate::local_model::DevRuntimeKind::Native
        );
    }

    #[test]
    fn cpu_manifest_rejects_same_size_library_tamper_against_receipt_pin() {
        use crate::local_model::PARAKEET_ENTRY;
        let state = tempfile::tempdir().unwrap();
        let layout = ConsentedLayout::at(state.path().join("models/consented/parakeet/hash"));
        let pins = catalog_pins(&PARAKEET_ENTRY);
        let library = layout.root.join(crate::local_model::NEMO_LIBRARY);
        std::fs::create_dir_all(library.parent().unwrap()).unwrap();
        std::fs::write(&library, b"TAMPER!").unwrap();
        let python = layout.venv_python();
        std::fs::create_dir_all(python.parent().unwrap()).unwrap();
        std::fs::write(python, b"python").unwrap();
        assert!(
            prepare_manifest(
                &layout,
                &pins,
                state.path(),
                &PARAKEET_ENTRY,
                &BTreeMap::new(),
            )
            .is_err()
        );
        let runtime_manifest = native_library_runtime_manifest(b"fixture");
        let (manifest, _, _) = prepare_manifest(
            &layout,
            &pins,
            state.path(),
            &PARAKEET_ENTRY,
            &runtime_manifest,
        )
        .unwrap();
        let pin = manifest
            .files
            .iter()
            .find(|pin| pin.path == library.to_str().unwrap())
            .expect("native library pin");
        assert_eq!(pin.sha256, sha256_hex(b"fixture"));
        assert_ne!(pin.sha256, sha256_hex(b"TAMPER!"));
        assert_eq!(pin.bytes, 7);
    }

    fn native_library_runtime_manifest(bytes: &[u8]) -> BTreeMap<String, ConsentedRuntimeEntry> {
        let key = crate::local_model::NEMO_LIBRARY
            .strip_prefix("runtime/")
            .expect("runtime-relative native library");
        BTreeMap::from([(
            key.into(),
            ConsentedRuntimeEntry::File {
                bytes: bytes.len() as u64,
                sha256: sha256_hex(bytes),
            },
        )])
    }

    #[test]
    fn cpu_health_requires_cpu_ready_and_reaps() {
        for (device, accepted) in [("cpu", true), ("cuda:0", false), ("unknown", false)] {
            let mut worker = HealthWorker {
                inner: super::super::FakeWorker {
                    model_receipt_hash: "h".into(),
                    observed_device: device.into(),
                    ..Default::default()
                },
                shutdown: false,
                reaped: false,
            };
            assert_eq!(
                check_worker_health_on_device(&mut worker, "h", "cpu").is_ok(),
                accepted
            );
            assert!(worker.reaped);
        }
    }

    #[test]
    fn consented_generated_manifest_is_private_hashed_and_outside_writable_cache() {
        use std::os::unix::fs::PermissionsExt;
        let state = tempfile::tempdir().unwrap();
        let layout = ConsentedLayout::at(state.path().join("models/consented/distil/hash"));
        let pins = catalog_pins(&CONSENTED_ENTRY);
        for name in pins.keys() {
            let path = layout.root.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"fixture").unwrap();
        }
        let python = layout.venv_python();
        std::fs::create_dir_all(python.parent().unwrap()).unwrap();
        std::fs::write(&python, b"fake Python").unwrap();
        let (manifest, hash, file) = prepare_manifest(
            &layout,
            &pins,
            state.path(),
            &CONSENTED_ENTRY,
            &BTreeMap::new(),
        )
        .unwrap();
        let bytes = std::fs::read(file.path()).unwrap();
        assert_eq!(
            file.as_file().metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(hash, sha256_hex(&bytes));
        assert_eq!(
            DevManifest::from_json(std::str::from_utf8(&bytes).unwrap()).unwrap(),
            manifest
        );
        let cache = Path::new(manifest.options["cache_dir"].as_str().unwrap());
        assert!(file.path().starts_with(state.path()));
        assert!(!file.path().starts_with(cache));
        assert!(!cache.starts_with(&layout.root));
        assert_eq!(
            manifest.files.last().unwrap().sha256,
            sha256_hex(b"fake Python")
        );
    }

    #[test]
    fn health_error_maps_gpu_busy_without_a_health_failure() {
        assert!(matches!(
            health_error(SupervisorError::Busy),
            ConsentedError::GpuBusy
        ));
        assert!(matches!(
            health_error(SupervisorError::Unavailable(GPU_BUSY)),
            ConsentedError::GpuBusy
        ));
        assert!(matches!(
            health_error(SupervisorError::Crashed),
            ConsentedError::Health(_)
        ));
        assert!(matches!(
            health_error(SupervisorError::Unavailable("cannot lock Local GPU")),
            ConsentedError::Health(_)
        ));
    }

    #[test]
    fn consented_health_requires_cuda_ready_and_always_reaps() {
        for (device, refusal, reap, accepted) in [
            ("cuda:0", None, ReapOutcome::Exited, true),
            ("cpu", None, ReapOutcome::Exited, false),
            ("cuda:0", Some("health_failed"), ReapOutcome::Exited, false),
            ("cuda:0", None, ReapOutcome::Unreaped, false),
        ] {
            let mut worker = HealthWorker {
                inner: super::super::FakeWorker {
                    model_receipt_hash: "h".into(),
                    observed_device: device.into(),
                    scripted_error: refusal.map(str::to_owned),
                    reap,
                    ..Default::default()
                },
                shutdown: false,
                reaped: false,
            };
            let result = check_worker_health(&mut worker, "h");
            assert_eq!(result.is_ok(), accepted);
            if reap == ReapOutcome::Unreaped {
                assert!(matches!(result, Err(ConsentedError::HealthUnreaped)));
            }
            assert!(worker.reaped);
            assert_eq!(worker.shutdown, device == "cuda:0" && refusal.is_none());
        }
    }

    struct HealthWorker {
        inner: super::super::FakeWorker,
        shutdown: bool,
        reaped: bool,
    }
    impl WorkerChild for HealthWorker {
        fn exchange(
            &mut self,
            control: ControlFrame,
            pcm: Option<&[u8]>,
            deadline: std::time::Instant,
        ) -> Result<WorkerFrame, SupervisorError> {
            self.shutdown |= matches!(control, ControlFrame::Cancel { .. });
            self.inner.exchange(control, pcm, deadline)
        }
        fn cancel_and_reap(&mut self) -> Result<ReapOutcome, SupervisorError> {
            self.reaped = true;
            self.inner.cancel_and_reap()
        }
    }

    #[test]
    fn consented_manifest_matches_the_worker_contract() {
        let layout = ConsentedLayout::at("/state/models/consented/distil/hash".into());
        let pins = std::collections::BTreeMap::from([
            (
                "jfk.wav".into(),
                ConsentedFilePin {
                    sha256: "a".repeat(64),
                    bytes: 22,
                },
            ),
            (
                "weights/model.bin".into(),
                ConsentedFilePin {
                    sha256: "b".repeat(64),
                    bytes: 33,
                },
            ),
            (
                "worker/worker.py".into(),
                ConsentedFilePin {
                    sha256: "c".repeat(64),
                    bytes: 44,
                },
            ),
        ]);
        let python = DevFilePin {
            path: layout.venv_python().to_str().unwrap().into(),
            sha256: "d".repeat(64),
            bytes: 55,
        };
        let manifest = manifest_from_pins(
            &layout,
            &pins,
            python,
            resolve_worker_limits(16 * 1024 * 1024 * 1024, 12),
            Path::new("/state/worker-cache"),
            &CONSENTED_ENTRY,
        );
        assert_eq!(
            serde_json::to_value(manifest).unwrap(),
            json!({
                "version": 1, "model_id": "distil-large-v3.5-int8", "device": "cuda:0", "language": "en",
                "runtime": {"python": "/state/models/consented/distil/hash/runtime/venv/bin/python", "worker": "/state/models/consented/distil/hash/worker/worker.py", "revision": "9793ccc07920e0f830e1dba0343efcdf0ef8c903"},
                "model_path": "/state/models/consented/distil/hash/weights",
                "files": [
                    {"path": "/state/models/consented/distil/hash/jfk.wav", "sha256": "a".repeat(64), "bytes": 22},
                    {"path": "/state/models/consented/distil/hash/weights/model.bin", "sha256": "b".repeat(64), "bytes": 33},
                    {"path": "/state/models/consented/distil/hash/worker/worker.py", "sha256": "c".repeat(64), "bytes": 44},
                    {"path": "/state/models/consented/distil/hash/runtime/venv/bin/python", "sha256": "d".repeat(64), "bytes": 55}
                ],
                "options": {"revision": "9793ccc07920e0f830e1dba0343efcdf0ef8c903", "compute_type": "int8_float16", "health_audio": "/state/models/consented/distil/hash/jfk.wav", "health_expected": "And so my fellow Americans ask not what your country can do for you. Ask what you can do for your country.", "cache_dir": "/state/worker-cache"},
                "limits": {"memory_bytes": 4294967296_u64, "tasks": 64, "cpu_cores": 4, "cache_bytes": 268435456, "vram_bytes": 2147483648_u64}
            })
        );
    }
}
