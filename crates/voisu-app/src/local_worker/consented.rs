//! Offline worker for an explicitly consented install.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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
    DevRuntimeManifest, EXPECTED_HEALTH_TEXT, VerifiedConsentedInstall, consented_worker_files,
    development_catalog_entry, ensure_private_dir, sha256_hex,
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
        let device = manifest.device.clone();
        let mut worker =
            DevWorker::launch_consented(manifest, hash.clone(), file, &layout.root, None, lease)
                .map_err(health_error)?;
        check_worker_health_on_device(&mut worker, &hash, &device)
    }

    /// Before any download: the packaged whisper.cpp worker must exist, and
    /// when `local_device` asks for the GPU it must see one. A GPU-less
    /// machine on the default `auto` learns that now, not after 818 MB.
    fn preflight(&self, entry: &ConsentedCatalogEntry) -> Result<(), ConsentedError> {
        if !entry.runs_whisper_cpp() {
            return Ok(());
        }
        let worker = whisper_cpp_worker()?;
        if launch_device(entry) == "gpu" {
            probe_gpu(&worker, GPU_PROBE_DEADLINE)?;
        }
        Ok(())
    }
}

/// Whether the packaged whisper.cpp worker sees a GPU, so setup can say where
/// the install will run before asking. A missing worker counts as found: the
/// install's preflight then reports the real problem.
#[must_use]
pub fn whisper_cpp_finds_gpu() -> bool {
    whisper_cpp_worker().map_or(true, |worker| {
        probe_gpu(&worker, GPU_PROBE_DEADLINE).is_ok()
    })
}

/// Loading the Vulkan driver takes well under a second; a hung driver must
/// not hang the install.
const GPU_PROBE_DEADLINE: Duration = Duration::from_secs(20);
/// How long a killed probe, and the pipes of an exited one, may take to close.
const GPU_PROBE_REAP_GRACE: Duration = Duration::from_secs(1);
/// What a probe may print; the rest is drained unread.
const GPU_PROBE_OUTPUT_BYTES: u64 = 16 * 1024;

/// `voisu-local-asr --probe-gpu` with nothing from the environment that could
/// load other code into it, so it sees what the sandboxed worker will.
fn probe_command(worker: &Path) -> std::process::Command {
    let mut command = std::process::Command::new(worker);
    command
        .arg("--probe-gpu")
        .env_remove("GGML_BACKEND_PATH")
        .env_remove("LD_LIBRARY_PATH")
        .env_remove("LD_PRELOAD")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    command
}

fn probe_gpu(worker: &Path, deadline: Duration) -> Result<(), ConsentedError> {
    let mut child = probe_command(worker).spawn().map_err(|error| {
        ConsentedError::Health(format!(
            "gpu_unavailable: cannot run the GPU probe: {error}"
        ))
    })?;
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut text = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = (&mut pipe)
                    .take(GPU_PROBE_OUTPUT_BYTES)
                    .read_to_end(&mut text);
                let _ = std::io::copy(&mut pipe, &mut std::io::sink());
            }
            String::from_utf8_lossy(&text).trim().to_owned()
        })
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
    );
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if started.elapsed() >= deadline {
            let _ = child.kill();
            // A child stuck in the kernel can outlive SIGKILL; it is reaped
            // off this thread rather than block the install.
            let killed = Instant::now() + GPU_PROBE_REAP_GRACE;
            loop {
                match child.try_wait() {
                    Ok(None) if Instant::now() < killed => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Ok(None) => {
                        std::thread::spawn(move || {
                            let _ = child.wait();
                        });
                        break;
                    }
                    Ok(Some(_)) | Err(_) => break,
                }
            }
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    // Anything the probe started may still hold its pipes open; a drain that
    // has not seen EOF by then is left running and its output dropped.
    let drained = Instant::now() + GPU_PROBE_REAP_GRACE;
    let (stdout, stderr) = (join_by(stdout, drained), join_by(stderr, drained));
    match status {
        Some(status) if status.success() && !stdout.is_empty() => Ok(()),
        Some(_) => {
            let reason = stderr.lines().last().unwrap_or("no GPU backend device");
            Err(ConsentedError::Health(format!(
                "gpu_unavailable: {reason}; choose `voisu local-device cpu` to run on the CPU"
            )))
        }
        None => Err(ConsentedError::Health(format!(
            "gpu_unavailable: the GPU probe did not finish within {}s",
            deadline.as_secs()
        ))),
    }
}

/// The drain's text if it finished by `by`; otherwise the thread is detached.
fn join_by(drain: std::thread::JoinHandle<String>, by: Instant) -> String {
    while !drain.is_finished() && Instant::now() < by {
        std::thread::sleep(Duration::from_millis(10));
    }
    if drain.is_finished() {
        drain.join().unwrap_or_default()
    } else {
        String::new()
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
                ..
            } if observed == correlation
                && super::dev::requested_device_matches(device, &observed_device) =>
            {
                Ok(())
            }
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

#[cfg(not(test))]
pub(crate) fn verify_default_consented_install() -> Result<VerifiedConsentedInstall, ConsentedError>
{
    let dir = crate::local_model::models_dir_from_state(
        std::env::var_os("XDG_STATE_HOME"),
        std::env::var_os("HOME"),
    )
    .map_err(ConsentedError::Store)?;
    let store = crate::local_model::ModelStore::open(dir).map_err(ConsentedError::Store)?;
    verify_consented_install_refreshing(&store, || {})
}

/// Worker-launch admission: stale worker files are rewritten from this binary
/// and health-checked first (ADR 0012). Only the process that launches workers
/// calls this; status paths stay read-only. The state directory is the store's
/// parent, as `launch_consented_worker` derives it.
#[cfg(not(test))]
pub(crate) fn verify_consented_install_refreshing(
    store: &crate::local_model::ModelStore,
    on_verifying: impl FnOnce(),
) -> Result<VerifiedConsentedInstall, ConsentedError> {
    let entry = crate::local_model::consented::installed_entry(store)?;
    let state = store.root().parent().ok_or(ConsentedError::Receipt)?;
    crate::local_model::consented::verify_with_entry_refreshing(
        store,
        entry,
        &ConsentedWorkerHealth::for_entry(state.to_path_buf(), entry),
        on_verifying,
    )
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
    for (name, bytes) in consented_worker_files()
        .iter()
        .filter(|_| !entry.runs_whisper_cpp())
    {
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
    let device = launch_device(entry);
    let cache = state.join("consented-worker/cache");
    if cache.starts_with(&layout.root) {
        return Err(ConsentedError::Tree(
            "cache must be outside the read-only install".into(),
        ));
    }
    ensure_private_dir(&cache).map_err(ConsentedError::Fs)?;
    // The program systemd-run starts: the install's venv interpreter, or the
    // packaged whisper.cpp worker. Either is pinned by its current digest.
    let runner_path = if entry.runs_whisper_cpp() {
        whisper_cpp_worker()?
    } else {
        layout.venv_python()
    };
    let mut runner = std::fs::File::open(&runner_path)?;
    let mut digest = Sha256::new();
    let mut buf = [0_u8; 64 * 1024];
    loop {
        let size = runner.read(&mut buf)?;
        if size == 0 {
            break;
        }
        digest.update(&buf[..size]);
    }
    let pin = DevFilePin {
        path: runner_path
            .to_str()
            .ok_or_else(|| ConsentedError::Tree("worker path must be UTF-8".into()))?
            .to_owned(),
        sha256: format!("{:x}", digest.finalize()),
        bytes: runner.metadata()?.len(),
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
    let manifest = manifest_from_pins(layout, &pins, pin, limits, &cache, entry, device);
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

/// Where the worker runs. whisper.cpp follows the `local_device` setting;
/// the Python entries are pinned to their catalog device.
fn launch_device(entry: &ConsentedCatalogEntry) -> &'static str {
    if entry.runs_whisper_cpp() {
        crate::config::local_device().manifest_device()
    } else {
        entry.device
    }
}

/// The packaged whisper.cpp worker. No environment override: the daemon only
/// starts a worker it can name from its own install.
const PACKAGED_WHISPER_CPP_WORKER: &str = "/usr/libexec/voisu/voisu-local-asr";
/// Arch keeps private helpers in `/usr/lib/<pkg>/`, not `/usr/libexec/`.
const ARCH_WHISPER_CPP_WORKER: &str = "/usr/lib/voisu/voisu-local-asr";
/// Every build of the worker writes this list of its private ggml libraries
/// beside itself. A `voisu-local-asr` without it is not one Voisu built (a
/// stray `cargo install` copy, say) and would load the system libggml.
const WHISPER_CPP_RUNTIME_LIST: &str = "voisu-whisper-runtime.txt";

/// The packaged worker sits in a private directory with its ggml libraries
/// and backend modules. Packaged paths win; the worker beside this binary is
/// used only for a cargo build (an ancestor named `target`), so a stray copy
/// in `~/.cargo/bin` or `/usr/bin` is never pinned.
fn whisper_cpp_worker() -> Result<PathBuf, ConsentedError> {
    let exe = std::env::current_exe().ok();
    pick_whisper_cpp_worker(
        &[
            Path::new(PACKAGED_WHISPER_CPP_WORKER),
            Path::new(ARCH_WHISPER_CPP_WORKER),
        ],
        exe.as_deref(),
    )
    .ok_or_else(|| {
        ConsentedError::Health(format!(
            "whisper.cpp worker missing: install the Voisu package that ships {PACKAGED_WHISPER_CPP_WORKER}"
        ))
    })
}

fn pick_whisper_cpp_worker(packaged: &[&Path], exe: Option<&Path>) -> Option<PathBuf> {
    let dev = exe.and_then(|exe| {
        let dir = exe.parent()?;
        dir.ancestors()
            .any(|ancestor| ancestor.file_name().is_some_and(|name| name == "target"))
            .then(|| dir.join("voisu-local-asr"))
    });
    packaged
        .iter()
        .map(|path| path.to_path_buf())
        .chain(dev)
        .find(|path| {
            path.is_file()
                && path
                    .parent()
                    .is_some_and(|dir| dir.join(WHISPER_CPP_RUNTIME_LIST).is_file())
        })
}

fn manifest_from_pins(
    layout: &ConsentedLayout,
    pins: &BTreeMap<String, ConsentedFilePin>,
    runner: DevFilePin,
    limits: WorkerResourceLimits,
    cache: &Path,
    entry: &ConsentedCatalogEntry,
    device: &str,
) -> DevManifest {
    let mut files = pins
        .iter()
        .map(|(name, pin)| DevFilePin {
            path: layout.root.join(name).to_string_lossy().into_owned(),
            sha256: pin.sha256.clone(),
            bytes: pin.bytes,
        })
        .collect::<Vec<_>>();
    let whisper_cpp = entry.runs_whisper_cpp();
    let runtime = if whisper_cpp {
        DevRuntimeManifest {
            python: None,
            worker: runner.path.clone(),
            revision: entry.revision.into(),
        }
    } else {
        DevRuntimeManifest {
            python: Some(runner.path.clone()),
            worker: layout
                .worker
                .join("worker.py")
                .to_string_lossy()
                .into_owned(),
            revision: entry.revision.into(),
        }
    };
    files.push(runner);
    DevManifest {
        version: 1,
        model_id: entry.id.into(),
        device: device.into(),
        language: "en".into(),
        runtime,
        model_path: if entry.native_runtime.is_some() || whisper_cpp {
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
            if whisper_cpp {
                options.remove("revision");
                options.remove("compute_type");
                if let Some(decode) = entry.whisper_cpp_decode {
                    options.insert("beam_size".into(), serde_json::json!(decode.beam_size));
                    options.insert("flash_attn".into(), serde_json::json!(decode.flash_attn));
                }
            }
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
            vram_bytes: if device == "cpu" {
                0
            } else {
                development_catalog_entry(entry.id)
                    .expect("GPU catalog entry")
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
    fn the_gpu_probe_passes_only_a_worker_that_names_a_gpu_in_time() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = |name: &str, body: &str| {
            let path = dir.path().join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        let deadline = Duration::from_secs(5);
        let gpu = script(
            "gpu",
            r#"[ "$1" = --probe-gpu ] && echo 'vulkan:0 Test GPU'"#,
        );
        probe_gpu(&gpu, deadline).unwrap();

        let none = script(
            "none",
            "echo 'voisu-asr: gpu_unavailable: no GPU backend device loaded' >&2; exit 3",
        );
        let ConsentedError::Health(reason) = probe_gpu(&none, deadline).unwrap_err() else {
            panic!("a GPU-less probe is a health failure");
        };
        assert!(
            reason.starts_with("gpu_unavailable: voisu-asr: gpu_unavailable"),
            "{reason}"
        );
        assert!(reason.contains("voisu local-device cpu"), "{reason}");

        let silent = script("silent", "exit 0");
        assert!(
            probe_gpu(&silent, deadline).is_err(),
            "success must name a GPU"
        );

        let hung = script("hung", "exec sleep 30");
        let started = Instant::now();
        let ConsentedError::Health(reason) =
            probe_gpu(&hung, Duration::from_millis(200)).unwrap_err()
        else {
            panic!("a hung probe is a health failure");
        };
        assert!(reason.contains("did not finish"), "{reason}");
        assert!(started.elapsed() < Duration::from_secs(5));

        // A process the probe left behind keeps its pipes open after the
        // probe is killed; the drains must not wait for it.
        let orphan = script("orphan", "sleep 30 &\nexec sleep 30");
        let started = Instant::now();
        let ConsentedError::Health(reason) =
            probe_gpu(&orphan, Duration::from_millis(200)).unwrap_err()
        else {
            panic!("a hung probe is a health failure");
        };
        assert!(reason.contains("did not finish"), "{reason}");
        assert!(started.elapsed() < Duration::from_secs(5));

        // Nothing from the daemon's environment may load code into the probe.
        let removed = probe_command(&gpu)
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            removed,
            ["GGML_BACKEND_PATH", "LD_LIBRARY_PATH", "LD_PRELOAD"]
        );
    }

    #[test]
    fn the_whisper_cpp_worker_is_packaged_first_and_needs_its_runtime_list() {
        let root = tempfile::tempdir().unwrap();
        let place = |dir: &str, list: bool| {
            let dir = root.path().join(dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("voisu-local-asr"), b"elf").unwrap();
            if list {
                std::fs::write(dir.join(WHISPER_CPP_RUNTIME_LIST), b"libwhisper.so.1\n").unwrap();
            }
            dir.join("voisu-local-asr")
        };
        let packaged = place("usr/libexec/voisu", true);
        let arch = place("usr/lib/voisu", true);
        let dev_exe = place("src/voisu/target/debug", true).with_file_name("voisu");
        let stray_exe = place("home/.cargo/bin", true).with_file_name("voisu");
        let unlisted = place("opt/voisu", false);
        let pick = |packaged: &[&Path], exe: &Path| pick_whisper_cpp_worker(packaged, Some(exe));

        assert_eq!(
            pick(&[&packaged, &arch], &dev_exe),
            Some(packaged.clone()),
            "package beats dev"
        );
        assert_eq!(
            pick(&[&unlisted, &arch], &dev_exe),
            Some(arch.clone()),
            "no list, no pin"
        );
        assert_eq!(
            pick(&[], &dev_exe),
            Some(dev_exe.with_file_name("voisu-local-asr")),
            "a cargo target dir may use its own worker"
        );
        assert_eq!(
            pick(&[], &stray_exe),
            None,
            "a stray ~/.cargo/bin worker is never pinned"
        );
        let unlisted_dev = place("src/other/target/release", false).with_file_name("voisu");
        assert_eq!(
            pick(&[], &unlisted_dev),
            None,
            "a dev worker still needs its list"
        );
    }

    #[test]
    fn whisper_cpp_manifest_runs_the_packaged_worker_on_the_chosen_device() {
        use crate::local_model::WHISPER_CPP_ENTRY;
        let layout = ConsentedLayout::at(PathBuf::from("/state/models/consented/w/hash"));
        let pins = catalog_pins(&WHISPER_CPP_ENTRY);
        assert_eq!(
            pins.keys().collect::<Vec<_>>(),
            ["jfk.wav", "weights/ggml-distil-large-v3.5-q8_0.bin"],
            "no Python worker files"
        );
        let worker = DevFilePin {
            path: PACKAGED_WHISPER_CPP_WORKER.into(),
            sha256: "e".repeat(64),
            bytes: 66,
        };
        for (device, vram) in [("gpu", 1536 * 1024 * 1024), ("cpu", 0)] {
            let manifest = manifest_from_pins(
                &layout,
                &pins,
                worker.clone(),
                resolve_worker_limits(16 * 1024 * 1024 * 1024, 12),
                Path::new("/state/worker-cache"),
                &WHISPER_CPP_ENTRY,
                device,
            );
            manifest.validate().unwrap();
            assert_eq!(manifest.device, device);
            assert_eq!(manifest.limits.vram_bytes, vram);
            assert_eq!(manifest.runtime.python, None);
            assert_eq!(manifest.runtime.worker, PACKAGED_WHISPER_CPP_WORKER);
            assert_eq!(
                manifest.model_path,
                "/state/models/consented/w/hash/weights/ggml-distil-large-v3.5-q8_0.bin"
            );
            assert!(
                manifest
                    .files
                    .iter()
                    .any(|pin| pin.path == PACKAGED_WHISPER_CPP_WORKER)
            );
            let mut options = manifest.options.keys().collect::<Vec<_>>();
            options.sort();
            assert_eq!(
                options,
                [
                    "beam_size",
                    "cache_dir",
                    "flash_attn",
                    "health_audio",
                    "health_expected"
                ]
            );
            // The distil q8_0 entry pins beam 5 with flash attention.
            assert_eq!(manifest.options["beam_size"], json!(5));
            assert_eq!(manifest.options["flash_attn"], json!(true));
        }
    }

    #[test]
    fn whisper_cpp_health_accepts_any_gpu_for_gpu_but_never_the_cpu() {
        for (requested, observed, accepted) in [
            ("gpu", "vulkan:0", true),
            ("gpu", "cuda:0", true),
            ("gpu", "cpu", false),
            ("cpu", "cpu", true),
            ("cpu", "vulkan:0", false),
        ] {
            let mut worker = HealthWorker {
                inner: super::super::FakeWorker {
                    model_receipt_hash: "h".into(),
                    observed_device: observed.into(),
                    reap: ReapOutcome::Exited,
                    ..Default::default()
                },
                shutdown: false,
                reaped: false,
            };
            assert_eq!(
                check_worker_health_on_device(&mut worker, "h", requested).is_ok(),
                accepted,
                "{requested} vs {observed}"
            );
        }
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
            CONSENTED_ENTRY.device,
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
