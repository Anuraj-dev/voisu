//! Development worker adapter.
//!
//! A development runtime is launched through the user manager so it does not
//! inherit the production daemon's restrictive process ancestry.  The unit is
//! unique to one worker and is stopped before the adapter is dropped or
//! replaced.  The worker still speaks the bounded stdin/stdout protocol and
//! is translated into the existing correlated [`WorkerChild`] seam.

use std::io::ErrorKind;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::local_model::{DevDevice, DevManifest};

use super::bounds::{CANCEL_GRACE, MAX_JSON_FRAME_BYTES, MAX_PCM_BYTES, REAP_OBSERVE};
use super::protocol::{
    ControlFrame, Correlation, FrameError, WorkerFrame, validate_pcm, validate_transcript,
};
use super::supervisor::{ReapOutcome, SupervisorError, WorkerChild};
use super::whisper_cli::WhisperCppWorker;

const SYSTEMD_RUN: &str = "systemd-run";
const SYSTEMCTL: &str = "systemctl";
const MAX_REPLY_BYTES: usize = MAX_JSON_FRAME_BYTES;
const MAX_ERROR_MESSAGE_BYTES: usize = 512;
static NEXT_UNIT: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub struct DevWorker {
    manifest: DevManifest,
    manifest_hash: String,
    unit: String,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    observed_device: String,
    resident: bool,
    inference_ms: u64,
    backend: Option<String>,
    peak_host_ram_bytes: Option<u64>,
    peak_gpu_memory_bytes: Option<u64>,
}

/// Runtime factory output used by the existing Caps Lock Local path.  The
/// production Whisper adapter remains available, while an explicit private
/// development manifest selects the supervised development adapter.
pub fn refuse_if_development_worker_loaded() -> Result<(), SupervisorError> {
    let units = active_development_worker_units()?;
    if units.is_empty() {
        Ok(())
    } else {
        Err(SupervisorError::Busy)
    }
}

pub enum RuntimeWorker {
    Whisper(WhisperCppWorker),
    Development(DevWorker),
}

impl WorkerChild for RuntimeWorker {
    fn exchange(
        &mut self,
        control: ControlFrame,
        pcm: Option<&[u8]>,
        deadline: Instant,
    ) -> Result<WorkerFrame, SupervisorError> {
        match self {
            Self::Whisper(worker) => worker.exchange(control, pcm, deadline),
            Self::Development(worker) => worker.exchange(control, pcm, deadline),
        }
    }

    fn cancel_and_reap(&mut self) -> Result<ReapOutcome, SupervisorError> {
        match self {
            Self::Whisper(worker) => worker.cancel_and_reap(),
            Self::Development(worker) => worker.cancel_and_reap(),
        }
    }

    fn last_inference_metrics(&self) -> super::supervisor::InferenceMetrics {
        match self {
            Self::Whisper(worker) => worker.last_inference_metrics(),
            Self::Development(worker) => worker.last_inference_metrics(),
        }
    }
}

impl DevWorker {
    pub fn launch(
        manifest: DevManifest,
        manifest_hash: impl Into<String>,
        manifest_path: &Path,
    ) -> Result<Self, SupervisorError> {
        manifest
            .verify_pins()
            .map_err(|_| SupervisorError::LoadAborted)?;
        let manifest_hash = manifest_hash.into();
        if manifest_hash.len() != 64 || !manifest_hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(SupervisorError::Protocol(FrameError::CorrelationMismatch));
        }
        let unit = format!(
            "voisu-dev-worker-{}-{}",
            std::process::id(),
            NEXT_UNIT.fetch_add(1, Ordering::Relaxed)
        );
        let cache = cache_path(&manifest)?;
        std::fs::create_dir_all(&cache).map_err(|_| SupervisorError::LoadAborted)?;
        let args = systemd_run_args(&unit, &cache, &manifest, manifest_path);
        let mut command = Command::new(SYSTEMD_RUN);
        command
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = command.spawn().map_err(|_| SupervisorError::LoadAborted)?;
        let stdin = child.stdin.take().ok_or(SupervisorError::LoadAborted)?;
        let stdout = child.stdout.take().ok_or(SupervisorError::LoadAborted)?;
        Ok(Self {
            manifest,
            manifest_hash,
            unit,
            child: Some(child),
            stdin: Some(stdin),
            stdout: Some(stdout),
            observed_device: "unknown".into(),
            resident: false,
            inference_ms: 0,
            backend: None,
            peak_host_ram_bytes: None,
            peak_gpu_memory_bytes: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(manifest: DevManifest, manifest_hash: impl Into<String>) -> Self {
        Self {
            manifest,
            manifest_hash: manifest_hash.into(),
            unit: "voisu-dev-worker-test".into(),
            child: None,
            stdin: None,
            stdout: None,
            observed_device: "unknown".into(),
            resident: false,
            inference_ms: 0,
            backend: None,
            peak_host_ram_bytes: None,
            peak_gpu_memory_bytes: None,
        }
    }

    #[must_use]
    pub fn observed_device(&self) -> &str {
        &self.observed_device
    }

    #[must_use]
    pub fn resident(&self) -> bool {
        self.resident
    }

    #[must_use]
    pub fn last_inference_ms(&self) -> u64 {
        self.inference_ms
    }

    fn record_resource_metrics(&mut self, value: &Value) {
        if let Some(backend) = value.get("backend").and_then(Value::as_str) {
            self.backend = Some(backend.to_owned());
        }
        if let Some(bytes) = value.get("peak_host_ram_bytes").and_then(Value::as_u64) {
            self.peak_host_ram_bytes = Some(bytes);
        }
        if let Some(bytes) = value.get("peak_gpu_memory_bytes").and_then(Value::as_u64) {
            self.peak_gpu_memory_bytes = Some(bytes);
        }
    }

    fn exchange_json(
        &mut self,
        request: Value,
        pcm: Option<&[u8]>,
        deadline: Instant,
    ) -> Result<Value, SupervisorError> {
        let bytes = serde_json::to_vec(&request)
            .map_err(|_| SupervisorError::Protocol(FrameError::InvalidJson))?;
        if bytes.is_empty() || bytes.len() > MAX_JSON_FRAME_BYTES {
            return Err(SupervisorError::Protocol(FrameError::JsonTooLarge {
                bytes: bytes.len(),
            }));
        }
        let mut frame = Vec::with_capacity(4 + bytes.len() + pcm.map_or(0, <[u8]>::len));
        frame.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        frame.extend_from_slice(&bytes);
        if let Some(pcm) = pcm {
            validate_pcm(pcm, Some(pcm.len())).map_err(SupervisorError::Protocol)?;
            frame.extend_from_slice(pcm);
        }
        let stdin = self.stdin.as_mut().ok_or(SupervisorError::Crashed)?;
        write_until(stdin, &frame, deadline)?;
        let stdout = self.stdout.as_mut().ok_or(SupervisorError::Crashed)?;
        let mut header = [0u8; 4];
        read_until(stdout, &mut header, deadline)?;
        let size = u32::from_le_bytes(header) as usize;
        if size == 0 || size > MAX_REPLY_BYTES {
            return Err(SupervisorError::Protocol(FrameError::JsonTooLarge {
                bytes: size,
            }));
        }
        let mut payload = vec![0u8; size];
        read_until(stdout, &mut payload, deadline)?;
        let text = std::str::from_utf8(&payload)
            .map_err(|_| SupervisorError::Protocol(FrameError::InvalidUtf8))?;
        serde_json::from_str(text).map_err(|_| SupervisorError::Protocol(FrameError::InvalidJson))
    }

    fn map_reply(
        &mut self,
        value: Value,
        correlation: &Correlation,
    ) -> Result<WorkerFrame, SupervisorError> {
        let id = value
            .get("id")
            .and_then(Value::as_str)
            .ok_or(SupervisorError::Protocol(FrameError::InvalidJson))?;
        if id != correlation.request_id {
            return Err(SupervisorError::Protocol(FrameError::CorrelationMismatch));
        }
        match value.get("status").and_then(Value::as_str) {
            Some("ready") => {
                let observed = value
                    .get("observed_device")
                    .and_then(Value::as_str)
                    .ok_or(SupervisorError::Protocol(FrameError::InvalidJson))?;
                let resident = value
                    .get("resident")
                    .and_then(Value::as_bool)
                    .ok_or(SupervisorError::Protocol(FrameError::InvalidJson))?;
                if observed == "unknown"
                    || !requested_device_matches(&self.manifest.device, observed)
                {
                    return Err(SupervisorError::Unavailable(
                        "worker health did not prove requested device",
                    ));
                }
                if value.get("model_id").and_then(Value::as_str)
                    != Some(self.manifest.model_id.as_str())
                {
                    return Err(SupervisorError::Unavailable(
                        "worker reported a different model",
                    ));
                }
                self.observed_device = observed.to_owned();
                self.resident = resident;
                self.record_resource_metrics(&value);
                Ok(WorkerFrame::Ready {
                    correlation: correlation.clone(),
                    observed_device: observed.to_owned(),
                })
            }
            Some("transcript") => {
                self.require_stable_device(&value)?;
                self.observe_resident(&value);
                self.record_resource_metrics(&value);
                let text = value
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or(SupervisorError::Protocol(FrameError::InvalidJson))?;
                validate_transcript(text).map_err(SupervisorError::Protocol)?;
                self.inference_ms = value
                    .get("inference_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                Ok(WorkerFrame::Transcript {
                    correlation: correlation.clone(),
                    text: text.to_owned(),
                })
            }
            Some("no_text") => {
                self.require_stable_device(&value)?;
                self.observe_resident(&value);
                self.record_resource_metrics(&value);
                self.inference_ms = value
                    .get("inference_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                Ok(WorkerFrame::NoText {
                    correlation: correlation.clone(),
                    reason: bounded_string(
                        value
                            .get("reason")
                            .and_then(Value::as_str)
                            .unwrap_or("no_text"),
                    ),
                })
            }
            Some("error") => Ok(WorkerFrame::Error {
                code: bounded_string(
                    value
                        .get("code")
                        .and_then(Value::as_str)
                        .unwrap_or("runtime_error"),
                ),
                metadata: bounded_metadata(
                    value
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("development runtime error"),
                ),
            }),
            _ => Err(SupervisorError::Protocol(FrameError::InvalidJson)),
        }
    }

    fn require_stable_device(&self, value: &Value) -> Result<(), SupervisorError> {
        let observed = value
            .get("observed_device")
            .and_then(Value::as_str)
            .ok_or(SupervisorError::Protocol(FrameError::InvalidJson))?;
        if !requested_device_matches(&self.manifest.device, observed)
            || observed != self.observed_device
        {
            return Err(SupervisorError::Unavailable("inference device changed"));
        }
        Ok(())
    }

    fn observe_resident(&mut self, value: &Value) {
        if let Some(resident) = value.get("resident").and_then(Value::as_bool) {
            self.resident = resident;
        }
    }

    fn unit_is_active(&self) -> bool {
        Command::new(SYSTEMCTL)
            .args(["--user", "is-active", "--quiet", &self.unit])
            .status()
            .map(|status| status.success())
            .unwrap_or(true)
    }

    fn unit_stop(&mut self) -> ReapOutcome {
        let stop_ok = Command::new(SYSTEMCTL)
            .args(["--user", "stop", &self.unit])
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        let Some(mut child) = self.child.take() else {
            self.stdin.take();
            self.stdout.take();
            if !stop_ok || self.unit_is_active() {
                return ReapOutcome::Unreaped;
            }
            return ReapOutcome::Exited;
        };
        self.stdin.take();
        self.stdout.take();
        let started = Instant::now();
        while started.elapsed() < REAP_OBSERVE {
            if child.try_wait().ok().flatten().is_some() {
                if !stop_ok || self.unit_is_active() {
                    return ReapOutcome::Unreaped;
                }
                return ReapOutcome::Exited;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = child.kill();
        let deadline = Instant::now() + CANCEL_GRACE;
        while Instant::now() < deadline {
            if child.try_wait().ok().flatten().is_some() {
                if !stop_ok || self.unit_is_active() {
                    return ReapOutcome::Unreaped;
                }
                return ReapOutcome::Killed;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        ReapOutcome::Unreaped
    }
}

impl WorkerChild for DevWorker {
    fn last_inference_metrics(&self) -> super::supervisor::InferenceMetrics {
        super::supervisor::InferenceMetrics {
            backend: self.backend.clone(),
            resident: Some(self.resident),
            inference_ms: Some(self.inference_ms),
            peak_host_ram_bytes: self.peak_host_ram_bytes,
            peak_gpu_memory_bytes: self.peak_gpu_memory_bytes,
        }
    }

    fn exchange(
        &mut self,
        control: ControlFrame,
        pcm: Option<&[u8]>,
        deadline: Instant,
    ) -> Result<WorkerFrame, SupervisorError> {
        let (op, id, pcm_bytes) = match &control {
            ControlFrame::Prepare(correlation) => ("prepare", correlation.request_id.clone(), None),
            ControlFrame::Transcribe {
                correlation,
                pcm_bytes,
            } => (
                "transcribe",
                correlation.request_id.clone(),
                Some(*pcm_bytes),
            ),
            ControlFrame::Cancel { request_id } => ("shutdown", request_id.clone(), None),
        };
        let correlation = match control {
            ControlFrame::Prepare(correlation) | ControlFrame::Transcribe { correlation, .. } => {
                correlation
            }
            ControlFrame::Cancel { request_id } => Correlation {
                daemon_nonce: String::new(),
                generation: 0,
                request_id,
                recording_id: String::new(),
                model_receipt_hash: self.manifest_hash.clone(),
            },
        };
        if let Some(pcm_bytes) = pcm_bytes {
            if pcm.map_or(0, <[u8]>::len) != pcm_bytes || pcm_bytes > MAX_PCM_BYTES {
                return Err(SupervisorError::Protocol(FrameError::DeclaredPcmMismatch {
                    declared: pcm_bytes,
                    actual: pcm.map_or(0, <[u8]>::len),
                }));
            }
        }
        let mut request = json!({"v": 1, "op": op, "id": id});
        if let Some(pcm_bytes) = pcm_bytes {
            request["pcm_bytes"] = Value::from(pcm_bytes);
        }
        let reply = self.exchange_json(request, pcm, deadline)?;
        if op == "shutdown" {
            return Ok(WorkerFrame::Error {
                code: "cancelled".into(),
                metadata: Map::new(),
            });
        }
        self.map_reply(reply, &correlation)
    }

    fn cancel_and_reap(&mut self) -> Result<ReapOutcome, SupervisorError> {
        Ok(self.unit_stop())
    }
}

impl Drop for DevWorker {
    fn drop(&mut self) {
        if self.child.is_some() {
            let _ = self.unit_stop();
        }
    }
}

fn write_until(
    writer: &mut ChildStdin,
    bytes: &[u8],
    deadline: Instant,
) -> Result<(), SupervisorError> {
    let mut offset = 0;
    while offset < bytes.len() {
        wait_fd(writer.as_raw_fd(), libc::POLLOUT, deadline)?;
        // The poll above prevents a full pipe from making the daemon block;
        // use the raw descriptor so the write remains bounded even when a
        // frame is larger than the pipe buffer.
        let result = unsafe {
            libc::write(
                writer.as_raw_fd(),
                bytes[offset..].as_ptr().cast(),
                bytes.len() - offset,
            )
        };
        if result > 0 {
            offset += usize::try_from(result).map_err(|_| SupervisorError::Crashed)?;
        } else if result == 0 {
            return Err(SupervisorError::Crashed);
        } else {
            let error = std::io::Error::last_os_error();
            if error.kind() != ErrorKind::Interrupted {
                return Err(if error.kind() == ErrorKind::WouldBlock {
                    SupervisorError::TimedOut
                } else {
                    SupervisorError::Crashed
                });
            }
        }
    }
    Ok(())
}

fn read_until(
    reader: &mut ChildStdout,
    bytes: &mut [u8],
    deadline: Instant,
) -> Result<(), SupervisorError> {
    let mut offset = 0;
    while offset < bytes.len() {
        wait_fd(reader.as_raw_fd(), libc::POLLIN, deadline)?;
        let result = unsafe {
            libc::read(
                reader.as_raw_fd(),
                bytes[offset..].as_mut_ptr().cast(),
                bytes.len() - offset,
            )
        };
        if result > 0 {
            offset += usize::try_from(result).map_err(|_| SupervisorError::Crashed)?;
        } else if result == 0 {
            return Err(SupervisorError::Crashed);
        } else {
            let error = std::io::Error::last_os_error();
            if error.kind() != ErrorKind::Interrupted {
                return Err(if error.kind() == ErrorKind::WouldBlock {
                    SupervisorError::TimedOut
                } else {
                    SupervisorError::Crashed
                });
            }
        }
    }
    Ok(())
}

fn wait_fd(fd: std::os::fd::RawFd, events: i16, deadline: Instant) -> Result<(), SupervisorError> {
    let timeout = deadline.saturating_duration_since(Instant::now());
    if timeout.is_zero() {
        return Err(SupervisorError::TimedOut);
    }
    let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
    let mut pollfd = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    loop {
        let result = unsafe { libc::poll(&mut pollfd, 1, millis) };
        if result > 0 && pollfd.revents & (libc::POLLIN | libc::POLLOUT) != 0 {
            return Ok(());
        }
        if result == 0 {
            return Err(SupervisorError::TimedOut);
        }
        if result < 0 && std::io::Error::last_os_error().kind() == ErrorKind::Interrupted {
            continue;
        }
        return Err(SupervisorError::Crashed);
    }
}

fn bounded_string(value: &str) -> String {
    value.chars().take(MAX_ERROR_MESSAGE_BYTES).collect()
}

fn bounded_metadata(message: &str) -> Map<String, Value> {
    let mut metadata = Map::new();
    metadata.insert("message".into(), Value::String(bounded_string(message)));
    metadata
}

fn requested_device_matches(requested: &str, observed: &str) -> bool {
    match DevDevice::parse(requested) {
        Some(DevDevice::Cuda0) => observed == "cuda:0",
        Some(DevDevice::Cpu) => observed == "cpu",
        None => false,
    }
}

fn cache_path(manifest: &DevManifest) -> Result<PathBuf, SupervisorError> {
    let path = manifest
        .options
        .get("cache_dir")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| default_cache_path(Path::new(&manifest.model_path)));
    if !path.is_absolute() {
        return Err(SupervisorError::Unavailable(
            "development cache path must be absolute",
        ));
    }
    Ok(path)
}

fn default_cache_path(model_path: &Path) -> PathBuf {
    if model_path.is_dir() || model_path.extension().is_none() {
        model_path.join(".voisu-cache")
    } else {
        model_path
            .parent()
            .unwrap_or(model_path)
            .join(".voisu-cache")
    }
}

fn active_development_worker_units() -> Result<Vec<String>, SupervisorError> {
    let output = Command::new(SYSTEMCTL)
        .args([
            "--user",
            "list-units",
            "--type=service",
            "--state=active",
            "--plain",
            "--no-legend",
            "voisu-dev-worker-*",
        ])
        .output()
        .map_err(|_| SupervisorError::Unavailable("cannot list development worker units"))?;
    if !output.status.success() {
        return Err(SupervisorError::Unavailable(
            "cannot list development worker units",
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .filter_map(|line| {
            let name = line.split_whitespace().next()?;
            name.starts_with("voisu-dev-worker-")
                .then(|| name.trim().to_owned())
        })
        .collect())
}

fn systemd_run_args(
    unit: &str,
    cache: &Path,
    manifest: &DevManifest,
    manifest_path: &Path,
) -> Vec<String> {
    let mut args = vec![
        "--user".into(),
        "--pipe".into(),
        "--wait".into(),
        "--collect".into(),
        format!("--unit={unit}"),
        "--property=NoNewPrivileges=yes".into(),
        "--property=PrivateNetwork=yes".into(),
        "--property=RestrictAddressFamilies=AF_UNIX".into(),
        "--property=ProtectSystem=strict".into(),
        "--property=ProtectHome=read-only".into(),
        format!("--property=ReadWritePaths={}", cache.display()),
        format!("--property=MemoryMax={}", manifest.limits.memory_bytes),
        format!("--property=TasksMax={}", manifest.limits.tasks),
        format!(
            "--property=CPUQuota={}%",
            manifest.limits.cpu_cores.saturating_mul(100)
        ),
        "--property=RestrictSUIDSGID=yes".into(),
        "--property=UMask=0077".into(),
        "--setenv=PYTHONNOUSERSITE=1".into(),
        "--setenv=HF_HUB_OFFLINE=1".into(),
        "--setenv=TRANSFORMERS_OFFLINE=1".into(),
        "--setenv=HF_DATASETS_OFFLINE=1".into(),
        format!("--setenv=HOME={}", cache.display()),
        format!("--setenv=TMPDIR={}", cache.display()),
        format!("--setenv=TMP={}", cache.display()),
        format!("--setenv=TEMP={}", cache.display()),
        format!("--setenv=VOISU_DEV_DEVICE={}", manifest.device),
    ];
    if manifest.device == "cuda:0" {
        args.push("--setenv=CUDA_VISIBLE_DEVICES=0".into());
        args.push("--setenv=CUDA_DEVICE_ORDER=PCI_BUS_ID".into());
    }
    if let Some(library) = manifest
        .options
        .get("library")
        .and_then(Value::as_str)
        .map(Path::new)
        .and_then(Path::parent)
    {
        args.push(format!("--setenv=LD_LIBRARY_PATH={}", library.display()));
    }
    args.extend([
        "--".into(),
        manifest.runtime.python.clone(),
        "-I".into(),
        manifest.runtime.worker.clone(),
        "--manifest".into(),
        manifest_path.to_string_lossy().into_owned(),
    ]);
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_model::{DevLimits, DevRuntimeManifest};

    fn test_manifest(device: &str) -> DevManifest {
        let vram_bytes = if device == "cpu" { 0 } else { 1 };
        DevManifest {
            version: 1,
            model_id: "nemotron-3.5-0.6b".into(),
            device: device.into(),
            language: "en".into(),
            runtime: DevRuntimeManifest {
                python: "/bin/true".into(),
                worker: "/tmp/worker.py".into(),
                revision: "test".into(),
            },
            model_path: "/tmp/model.bin".into(),
            files: Vec::new(),
            options: serde_json::Map::new(),
            limits: DevLimits {
                memory_bytes: 1024,
                tasks: 4,
                cpu_cores: 1,
                cache_bytes: 1024,
                vram_bytes,
            },
        }
    }

    fn corr() -> Correlation {
        Correlation {
            daemon_nonce: "n".into(),
            generation: 1,
            request_id: "q".into(),
            recording_id: "r".into(),
            model_receipt_hash: "h".into(),
        }
    }

    #[test]
    fn default_cache_is_beside_a_model_file_not_under_it() {
        let file = Path::new("/tmp/model.bin");
        assert_eq!(default_cache_path(file), PathBuf::from("/tmp/.voisu-cache"));
        let dir = Path::new("/tmp/qwen-model");
        assert_eq!(
            default_cache_path(dir),
            PathBuf::from("/tmp/qwen-model/.voisu-cache")
        );
    }

    #[test]
    fn device_identity_is_exact_and_cpu_does_not_accept_cuda() {
        assert!(requested_device_matches("cuda:0", "cuda:0"));
        assert!(!requested_device_matches("cuda:0", "cpu"));
        assert!(requested_device_matches("cpu", "cpu"));
    }

    #[test]
    fn ready_allows_non_resident_cli_adapter_on_matching_device() {
        let mut worker = DevWorker::for_test(test_manifest("cpu"), "h");
        let frame = worker
            .map_reply(
                json!({
                    "id": "q",
                    "status": "ready",
                    "model_id": "nemotron-3.5-0.6b",
                    "observed_device": "cpu",
                    "resident": false
                }),
                &corr(),
            )
            .unwrap();
        assert!(matches!(
            frame,
            WorkerFrame::Ready {
                observed_device,
                ..
            } if observed_device == "cpu"
        ));
        assert!(!worker.resident());
        assert_eq!(worker.observed_device(), "cpu");
    }

    #[test]
    fn ready_rejects_missing_resident_as_invalid_json() {
        let mut worker = DevWorker::for_test(test_manifest("cpu"), "h");
        let error = worker
            .map_reply(
                json!({
                    "id": "q",
                    "status": "ready",
                    "model_id": "nemotron-3.5-0.6b",
                    "observed_device": "cpu"
                }),
                &corr(),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            SupervisorError::Protocol(FrameError::InvalidJson)
        ));
    }

    #[test]
    fn no_text_rejects_cpu_after_cuda_ready() {
        let mut worker = DevWorker::for_test(test_manifest("cuda:0"), "h");
        worker
            .map_reply(
                json!({
                    "id": "q",
                    "status": "ready",
                    "model_id": "nemotron-3.5-0.6b",
                    "observed_device": "cuda:0",
                    "resident": true
                }),
                &corr(),
            )
            .unwrap();
        let error = worker
            .map_reply(
                json!({
                    "id": "q",
                    "status": "no_text",
                    "reason": "silence",
                    "observed_device": "cpu"
                }),
                &corr(),
            )
            .unwrap_err();
        assert_eq!(
            error,
            SupervisorError::Unavailable("inference device changed")
        );
        assert_eq!(error.to_string(), "inference device changed");
    }

    #[test]
    fn systemd_run_isolates_network_and_applies_resource_limits() {
        let mut cuda = test_manifest("cuda:0");
        cuda.options.insert(
            "library".into(),
            Value::String("/opt/nemo/lib/libnemo_speech_asr_c.so.1".into()),
        );
        let cache = PathBuf::from("/tmp/voisu-dev-cache");
        let args = systemd_run_args(
            "voisu-dev-worker-test",
            &cache,
            &cuda,
            Path::new("/tmp/nemotron.json"),
        );
        assert!(
            args.iter()
                .any(|arg| arg == "--property=PrivateNetwork=yes")
        );
        assert!(
            args.iter().any(|arg| arg == "--property=MemoryMax=1024"
                || arg.starts_with("--property=MemoryMax="))
        );
        assert!(args.iter().any(|arg| arg == "--property=TasksMax=4"));
        assert!(args.iter().any(|arg| arg == "--property=CPUQuota=100%"));
        assert!(
            args.iter()
                .any(|arg| arg == "--setenv=CUDA_VISIBLE_DEVICES=0")
        );
        assert!(
            args.iter()
                .any(|arg| arg == "--setenv=CUDA_DEVICE_ORDER=PCI_BUS_ID")
        );
        assert!(
            args.iter()
                .any(|arg| arg == "--setenv=LD_LIBRARY_PATH=/opt/nemo/lib")
        );
        assert!(!args.iter().any(|arg| arg.contains("0.0.0.0")));
        assert!(args.iter().any(|arg| arg == "--setenv=HF_HUB_OFFLINE=1"));
        assert!(
            args.iter()
                .any(|arg| arg == "--setenv=TRANSFORMERS_OFFLINE=1")
        );
        assert!(
            !args
                .iter()
                .any(|arg| arg.to_ascii_lowercase().contains("vllm"))
        );
        let cpu = test_manifest("cpu");
        let cpu_args = systemd_run_args(
            "voisu-dev-worker-test",
            &cache,
            &cpu,
            Path::new("/tmp/cpu.json"),
        );
        assert!(
            cpu_args
                .iter()
                .any(|arg| arg == "--property=PrivateNetwork=yes")
        );
        assert!(
            !cpu_args
                .iter()
                .any(|arg| arg.starts_with("--setenv=CUDA_VISIBLE_DEVICES="))
        );
    }

    #[test]
    fn resident_stays_false_until_a_second_inference_reports_true() {
        let mut worker = DevWorker::for_test(test_manifest("cpu"), "h");
        worker
            .map_reply(
                json!({
                    "id": "q",
                    "status": "ready",
                    "model_id": "nemotron-3.5-0.6b",
                    "observed_device": "cpu",
                    "resident": false
                }),
                &corr(),
            )
            .unwrap();
        assert!(!worker.resident());
        worker
            .map_reply(
                json!({
                    "id": "q",
                    "status": "transcript",
                    "text": "ask not",
                    "observed_device": "cpu",
                    "inference_ms": 12
                }),
                &corr(),
            )
            .unwrap();
        assert!(!worker.resident());
        worker
            .map_reply(
                json!({
                    "id": "q",
                    "status": "transcript",
                    "text": "ask not",
                    "observed_device": "cpu",
                    "resident": true,
                    "inference_ms": 8
                }),
                &corr(),
            )
            .unwrap();
        assert!(worker.resident());
    }

    #[test]
    fn transcript_reply_keeps_backend_and_peak_memory() {
        let mut worker = DevWorker::for_test(test_manifest("cpu"), "h");
        worker
            .map_reply(
                json!({
                    "id": "q",
                    "status": "ready",
                    "model_id": "nemotron-3.5-0.6b",
                    "observed_device": "cpu",
                    "resident": false,
                    "backend": "nemo-speech.cpp",
                    "peak_host_ram_bytes": 2048,
                    "peak_gpu_memory_bytes": 0
                }),
                &corr(),
            )
            .unwrap();
        worker
            .map_reply(
                json!({
                    "id": "q",
                    "status": "transcript",
                    "text": "ask not",
                    "observed_device": "cpu",
                    "resident": true,
                    "inference_ms": 41,
                    "backend": "nemo-speech.cpp",
                    "peak_host_ram_bytes": 4096,
                    "peak_gpu_memory_bytes": 512
                }),
                &corr(),
            )
            .unwrap();
        let metrics = worker.last_inference_metrics();
        assert_eq!(metrics.backend.as_deref(), Some("nemo-speech.cpp"));
        assert_eq!(metrics.resident, Some(true));
        assert_eq!(metrics.inference_ms, Some(41));
        assert_eq!(metrics.peak_host_ram_bytes, Some(4096));
        assert_eq!(metrics.peak_gpu_memory_bytes, Some(512));
    }

    #[test]
    fn no_text_rejects_missing_observed_device() {
        let mut worker = DevWorker::for_test(test_manifest("cpu"), "h");
        worker
            .map_reply(
                json!({
                    "id": "q",
                    "status": "ready",
                    "model_id": "nemotron-3.5-0.6b",
                    "observed_device": "cpu",
                    "resident": true
                }),
                &corr(),
            )
            .unwrap();
        let error = worker
            .map_reply(
                json!({
                    "id": "q",
                    "status": "no_text",
                    "reason": "silence"
                }),
                &corr(),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            SupervisorError::Protocol(FrameError::InvalidJson)
        ));
    }
}
