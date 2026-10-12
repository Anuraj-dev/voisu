//! Offline CUDA worker for an explicitly consented install.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use sha2::{Digest, Sha256};

use super::bounds::{
    CANCEL_GRACE, LOAD_DEADLINE, WorkerResourceLimits, resolve_live_worker_limits,
};
use super::dev::DevWorker;
use super::protocol::{ControlFrame, Correlation, WorkerFrame};
use super::supervisor::{ReapOutcome, SupervisorError, WorkerChild};
#[cfg(not(test))]
use crate::local_model::{ActiveReceipt, FilePin};
use crate::local_model::{
    CONSENTED_ENTRY, ConsentedError, ConsentedFilePin, ConsentedHealth, ConsentedLayout,
    DevFilePin, DevLimits, DevManifest, DevRuntimeManifest, EXPECTED_HEALTH_TEXT, ModelStore,
    VerifiedConsentedInstall, consented_worker_files, development_catalog_entry,
    ensure_private_dir, models_dir_from_state, sha256_hex, verify_consented_install,
};

/// Installation's real, isolated CUDA health probe. `state_dir` is the Voisu
/// state directory (the parent of `models`), never the install tree.
pub struct ConsentedWorkerHealth {
    state_dir: PathBuf,
}

impl ConsentedWorkerHealth {
    pub fn new(state_dir: PathBuf) -> Self {
        Self { state_dir }
    }
}

impl ConsentedHealth for ConsentedWorkerHealth {
    fn check(&self, layout: &ConsentedLayout) -> Result<(), ConsentedError> {
        // The installer holds its exclusive store lock and has verified the
        // published tree; the receipt is committed only after this probe.
        let pins = catalog_pins();
        let (manifest, hash, file) = prepare_manifest(layout, &pins, &self.state_dir)?;
        let mut worker =
            DevWorker::launch_consented(manifest, hash.clone(), file, &layout.root, None)
                .map_err(health_error)?;
        check_worker_health(&mut worker, &hash)
    }
}

fn health_error(error: SupervisorError) -> ConsentedError {
    ConsentedError::Health(error.to_string())
}

fn check_worker_health(worker: &mut impl WorkerChild, hash: &str) -> Result<(), ConsentedError> {
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
            } if observed == correlation && observed_device == "cuda:0" => Ok(()),
            WorkerFrame::Error { code, .. } => Err(ConsentedError::Health(code)),
            _ => Err(ConsentedError::Health(
                "health did not prove ready on cuda:0".into(),
            )),
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
    let reaped = worker.cancel_and_reap().map_err(health_error)?;
    if reaped == ReapOutcome::Unreaped {
        return Err(ConsentedError::Health(
            "health worker unit was not reaped".into(),
        ));
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

#[cfg(not(test))]
pub(crate) fn consented_active_receipt(install: &VerifiedConsentedInstall) -> ActiveReceipt {
    let receipt = install.receipt();
    ActiveReceipt {
        catalog_id: receipt.entry_id.clone(),
        catalog_revision: receipt.revision.clone(),
        runtime_abi: format!("voisu-consented:{}", receipt.revision),
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
    let (manifest, hash, file) = prepare_manifest(layout, &install.receipt().files, state)?;
    let root = layout.root.clone();
    DevWorker::launch_consented(manifest, hash, file, &root, Some(install)).map_err(health_error)
}

fn catalog_pins() -> BTreeMap<String, ConsentedFilePin> {
    let mut pins = BTreeMap::new();
    for pin in CONSENTED_ENTRY.weights {
        pins.insert(
            format!("weights/{}", pin.name),
            ConsentedFilePin {
                sha256: pin.sha256.into(),
                bytes: pin.bytes,
            },
        );
    }
    let health = (CONSENTED_ENTRY.health_clip)();
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
    let manifest = manifest_from_pins(layout, pins, pin, limits, &cache);
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
        model_id: CONSENTED_ENTRY.id.into(),
        device: "cuda:0".into(),
        language: "en".into(),
        runtime: DevRuntimeManifest {
            python: layout.venv_python().to_string_lossy().into_owned(),
            worker: layout
                .worker
                .join("worker.py")
                .to_string_lossy()
                .into_owned(),
            revision: CONSENTED_ENTRY.revision.into(),
        },
        model_path: layout.weights.to_string_lossy().into_owned(),
        files,
        options: serde_json::json!({
            "revision": CONSENTED_ENTRY.revision,
            "compute_type": "int8_float16",
            "health_audio": layout.health_clip,
            "health_expected": EXPECTED_HEALTH_TEXT,
            "cache_dir": cache,
        })
        .as_object()
        .expect("object")
        .clone(),
        limits: DevLimits {
            memory_bytes: limits.memory_max_bytes,
            tasks: u64::from(limits.tasks_max),
            cpu_cores: limits.cpu_quota_cores,
            cache_bytes: limits.cache_max_bytes,
            vram_bytes: development_catalog_entry(CONSENTED_ENTRY.id)
                .expect("distil catalog entry")
                .vram_bytes,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::super::bounds::resolve_worker_limits;
    use super::*;
    use serde_json::json;

    #[test]
    fn consented_generated_manifest_is_private_hashed_and_outside_writable_cache() {
        use std::os::unix::fs::PermissionsExt;
        let state = tempfile::tempdir().unwrap();
        let layout = ConsentedLayout::at(state.path().join("models/consented/distil/hash"));
        let pins = catalog_pins();
        for name in pins.keys() {
            let path = layout.root.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"fixture").unwrap();
        }
        let python = layout.venv_python();
        std::fs::create_dir_all(python.parent().unwrap()).unwrap();
        std::fs::write(&python, b"fake Python").unwrap();
        let (manifest, hash, file) = prepare_manifest(&layout, &pins, state.path()).unwrap();
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
            assert_eq!(check_worker_health(&mut worker, "h").is_ok(), accepted);
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
