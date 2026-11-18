//! The private worker manifest the daemon writes before launch.
//!
//! The daemon validates the manifest's shape before it launches the worker
//! (`DevManifest::validate`). Like the Python worker, this re-checks what
//! the worker relies on from inside the sandbox: every pinned file is hashed
//! again, so a file swapped after the daemon looked is refused.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};
use voisu_core::{WavScan, scan_wav_pcm};

use crate::engine::{DecodeOptions, Device};

/// Widest beam the worker accepts.
const MAX_BEAM_SIZE: i32 = 8;

/// The beam without `options.beam_size`: the Phase 0 verdict for
/// distil-large-v3.5 (beam 5 beat greedy on accuracy at an affordable cost).
const DEFAULT_BEAM_SIZE: i64 = 5;

/// What the worker needs from a validated manifest.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkerManifest {
    pub model_id: String,
    /// The manifest's device: `cpu`, or `gpu` (the worker picks which).
    pub device: String,
    pub engine_device: Device,
    pub model_path: PathBuf,
    pub health_audio: PathBuf,
    pub health_expected: String,
    pub decode: DecodeOptions,
}

/// A manifest the worker refuses to serve; reported as `manifest_error`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestError(pub String);

fn refuse<T>(message: impl Into<String>) -> Result<T, ManifestError> {
    Err(ManifestError(message.into()))
}

fn absolute(value: Option<&Value>, label: &str) -> Result<PathBuf, ManifestError> {
    match value.and_then(Value::as_str).map(PathBuf::from) {
        Some(path) if path.is_absolute() => Ok(path),
        _ => refuse(format!("{label} must be an absolute path")),
    }
}

fn hash_file(path: &Path) -> Result<(String, u64), ManifestError> {
    let read_error = |error: std::io::Error| {
        ManifestError(format!(
            "cannot read pinned artifact {}: {error}",
            path.display()
        ))
    };
    let mut file = File::open(path).map_err(read_error)?;
    let mut digest = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut size = 0u64;
    loop {
        let read = file.read(&mut buf).map_err(read_error)?;
        if read == 0 {
            break;
        }
        digest.update(&buf[..read]);
        size += read as u64;
    }
    let hex = digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok((hex, size))
}

/// Every `files` entry is a regular file whose bytes still match its pin.
fn verified_pins(document: &Value) -> Result<Vec<PathBuf>, ManifestError> {
    let Some(files) = document
        .get("files")
        .and_then(Value::as_array)
        .filter(|f| !f.is_empty())
    else {
        return refuse("files must be a non-empty artifact list");
    };
    let mut pinned = Vec::with_capacity(files.len());
    for (index, entry) in files.iter().enumerate() {
        let path = absolute(entry.get("path"), &format!("files[{index}].path"))?;
        let sha256 = entry
            .get("sha256")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if sha256.len() != 64
            || !sha256
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            return refuse(format!(
                "files[{index}].sha256 must be a lowercase SHA-256 digest"
            ));
        }
        let Some(bytes) = entry.get("bytes").and_then(Value::as_u64) else {
            return refuse(format!(
                "files[{index}].bytes must be a non-negative integer"
            ));
        };
        if !path.is_file() {
            return refuse(format!(
                "pinned artifact is not a regular file: {}",
                path.display()
            ));
        }
        if hash_file(&path)? != (sha256.to_owned(), bytes) {
            return refuse(format!("pinned artifact changed: {}", path.display()));
        }
        pinned.push(path);
    }
    Ok(pinned)
}

fn positive(value: Option<&Value>) -> Option<i64> {
    value.and_then(Value::as_i64).filter(|value| *value > 0)
}

/// Read and verify the manifest at `path`. `running_worker`, when given, must
/// be the executable the manifest names as `runtime.worker`.
///
/// # Errors
///
/// Any shape, capability or pin the worker cannot serve.
pub fn load_manifest(
    path: &Path,
    running_worker: Option<&Path>,
) -> Result<WorkerManifest, ManifestError> {
    let raw =
        std::fs::read(path).map_err(|e| ManifestError(format!("cannot read manifest: {e}")))?;
    let document: Value = serde_json::from_slice(&raw)
        .map_err(|e| ManifestError(format!("cannot read manifest: {e}")))?;
    if !document.is_object() {
        return refuse("manifest must be a JSON object");
    }
    if document.get("version").and_then(Value::as_u64) != Some(1) {
        return refuse("manifest version must be 1");
    }
    let Some(model_id) = document
        .get("model_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
    else {
        return refuse("manifest model_id must be a non-empty string");
    };
    let device = document
        .get("device")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if document.get("language").and_then(Value::as_str) != Some("en") {
        return refuse("this worker serves language en only");
    }
    let limits = document.get("limits").unwrap_or(&Value::Null);
    let vram_bytes = limits
        .get("vram_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let engine_device = match (device, vram_bytes) {
        ("cpu", 0) => Device::Cpu,
        ("cpu", _) => return refuse("vram_bytes must be zero for CPU manifests"),
        ("gpu", 0) => return refuse("vram_bytes must be positive for GPU manifests"),
        ("gpu", vram_bytes) => Device::Gpu { vram_bytes },
        _ => return refuse("this worker serves device cpu or gpu only"),
    };

    let runtime = document.get("runtime").unwrap_or(&Value::Null);
    let worker = absolute(runtime.get("worker"), "runtime.worker")?;
    if let Some(running) = running_worker {
        let same = match (worker.canonicalize(), running.canonicalize()) {
            (Ok(named), Ok(running)) => named == running,
            _ => false,
        };
        if !same {
            return refuse("manifest worker does not match the running worker");
        }
    }
    let model_path = absolute(document.get("model_path"), "model_path")?;
    let options = document.get("options").unwrap_or(&Value::Null);
    let health_audio = absolute(options.get("health_audio"), "options.health_audio")?;
    let Some(health_expected) = options
        .get("health_expected")
        .and_then(Value::as_str)
        .filter(|phrase| !phrase.trim().is_empty())
    else {
        return refuse("options.health_expected must be a non-empty phrase");
    };

    let pinned = verified_pins(&document)?;
    for (required, label) in [
        (&worker, "runtime.worker"),
        (&model_path, "model_path"),
        (&health_audio, "options.health_audio"),
    ] {
        if !pinned.contains(required) {
            return refuse(format!("{label} is not pinned in files"));
        }
    }

    let cores = positive(limits.get("cpu_cores")).unwrap_or(1);
    let n_threads = positive(options.get("n_threads")).map_or(cores, |n| n.min(cores));
    let beam_size = match options.get("beam_size") {
        None => DEFAULT_BEAM_SIZE,
        Some(value) => match positive(Some(value)).filter(|n| *n <= i64::from(MAX_BEAM_SIZE)) {
            Some(beam) => beam,
            None => return refuse(format!("options.beam_size must be 1..={MAX_BEAM_SIZE}")),
        },
    };
    Ok(WorkerManifest {
        model_id: model_id.to_owned(),
        device: device.to_owned(),
        engine_device,
        model_path,
        health_audio,
        health_expected: health_expected.to_owned(),
        decode: DecodeOptions {
            n_threads: i32::try_from(n_threads).unwrap_or(i32::MAX),
            beam_size: i32::try_from(beam_size).unwrap_or(1),
        },
    })
}

/// The PCM16 samples of a mono 16 kHz health WAV.
///
/// # Errors
///
/// The file is unreadable or not s16 / 16 kHz / mono PCM.
pub fn read_health_wav(path: &Path) -> Result<Vec<i16>, ManifestError> {
    let bytes = std::fs::read(path)
        .map_err(|e| ManifestError(format!("health audio is not a readable WAV: {e}")))?;
    let start = match scan_wav_pcm(&bytes) {
        WavScan::DataAt(start) => start,
        WavScan::Incomplete => return refuse("health audio is not a complete WAV"),
        WavScan::Invalid(reason) => return refuse(format!("health audio: {reason}")),
    };
    // A file (unlike a stream) has a real data size; honor it when it fits.
    let declared = u32::from_le_bytes(bytes[start - 4..start].try_into().expect("4 bytes"));
    let end = start.saturating_add(declared as usize).min(bytes.len());
    let payload = &bytes[start..end];
    if payload.is_empty() || !payload.len().is_multiple_of(2) {
        return refuse("health audio PCM must be non-empty whole samples");
    }
    Ok(payload
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| i16::from_le_bytes(*pair))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Fixture {
        dir: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "voisu-local-asr-manifest-{name}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }

        fn file(&self, name: &str, bytes: &[u8]) -> Value {
            let path = self.dir.join(name);
            std::fs::write(&path, bytes).unwrap();
            let (sha256, size) = hash_file(&path).unwrap();
            json!({ "path": path, "sha256": sha256, "bytes": size })
        }

        fn manifest(&self, document: &Value) -> PathBuf {
            let path = self.dir.join("manifest.json");
            std::fs::write(&path, serde_json::to_vec(document).unwrap()).unwrap();
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn document(fixture: &Fixture, options: Value) -> Value {
        let files = vec![
            fixture.file("worker", b"worker"),
            fixture.file("model.bin", b"model"),
            fixture.file("health.wav", b"wav"),
        ];
        let mut options = options;
        options["health_audio"] = json!(fixture.dir.join("health.wav"));
        options["health_expected"] = json!("ask not");
        json!({
            "version": 1,
            "model_id": "whisper-cpp-test",
            "device": "cpu",
            "language": "en",
            "runtime": { "worker": fixture.dir.join("worker"), "revision": "r1" },
            "model_path": fixture.dir.join("model.bin"),
            "files": files,
            "options": options,
            "limits": { "cpu_cores": 4, "vram_bytes": 0 }
        })
    }

    #[test]
    fn threads_are_capped_by_cpu_cores_and_beam_defaults_to_five() {
        let fixture = Fixture::new("threads");
        let path = fixture.manifest(&document(&fixture, json!({ "n_threads": 16 })));
        let manifest = load_manifest(&path, None).unwrap();
        assert_eq!(
            manifest.decode,
            DecodeOptions {
                n_threads: 4,
                beam_size: 5
            }
        );
        assert_eq!(manifest.model_id, "whisper-cpp-test");
        assert_eq!(manifest.engine_device, Device::Cpu);
    }

    #[test]
    fn a_changed_pin_is_refused() {
        let fixture = Fixture::new("changed");
        let path = fixture.manifest(&document(&fixture, json!({ "beam_size": 5 })));
        assert_eq!(load_manifest(&path, None).unwrap().decode.beam_size, 5);
        std::fs::write(fixture.dir.join("model.bin"), b"swapped").unwrap();
        let error = load_manifest(&path, None).unwrap_err();
        assert!(error.0.starts_with("pinned artifact changed"), "{error:?}");
    }

    #[test]
    fn a_gpu_needs_its_memory_and_other_devices_or_workers_are_refused() {
        let fixture = Fixture::new("device");
        let mut doc = document(&fixture, json!({}));
        let path = fixture.manifest(&doc);
        let other = fixture.dir.join("model.bin");
        assert!(load_manifest(&path, Some(&other)).is_err());
        assert!(load_manifest(&path, Some(&fixture.dir.join("worker"))).is_ok());
        doc["device"] = json!("cuda:0");
        let path = fixture.manifest(&doc);
        assert!(load_manifest(&path, None).is_err());

        doc["device"] = json!("gpu");
        let path = fixture.manifest(&doc);
        let error = load_manifest(&path, None).unwrap_err();
        assert!(error.0.contains("positive for GPU"), "{error:?}");
        doc["limits"]["vram_bytes"] = json!(1u64 << 30);
        let path = fixture.manifest(&doc);
        assert_eq!(
            load_manifest(&path, None).unwrap().engine_device,
            Device::Gpu {
                vram_bytes: 1 << 30
            }
        );
        doc["device"] = json!("cpu");
        let path = fixture.manifest(&doc);
        assert!(load_manifest(&path, None).is_err());
    }
}
