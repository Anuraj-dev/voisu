//! Development worker adapter.
//!
//! A development runtime is launched through the user manager so it does not
//! inherit the production daemon's restrictive process ancestry.  The unit is
//! unique to one worker and is stopped before the adapter is dropped or
//! replaced.  The worker still speaks the bounded stdin/stdout protocol and
//! is translated into the existing correlated [`WorkerChild`] seam.

use std::fs::{File, OpenOptions};
use std::io::ErrorKind;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::local_model::consented::ConsentedLease;
use crate::local_model::{DevManifest, VerifiedConsentedInstall};

use super::bounds::{CANCEL_GRACE, MAX_JSON_FRAME_BYTES, MAX_PCM_BYTES, REAP_OBSERVE};
use super::protocol::{
    ControlFrame, Correlation, FrameError, WorkerFrame, validate_pcm, validate_transcript,
};
use super::supervisor::{ReapOutcome, SupervisorError, WorkerChild};
use super::whisper_cli::WhisperCppWorker;

const FLOCK: &str = "/usr/bin/flock";
const GPU_LOCK_CONFLICT: i32 = 73;
pub(crate) const GPU_BUSY: &str = "another Local worker holds the GPU";
const SYSTEMD_RUN: &str = "systemd-run";
const SYSTEMCTL: &str = "systemctl";
const MAX_REPLY_BYTES: usize = MAX_JSON_FRAME_BYTES;
const MAX_ERROR_MESSAGE_BYTES: usize = 512;
const DEV_WORKER_UNITS: &str = "voisu-dev-worker-*";
const CONSENTED_WORKER_UNITS: &str = "voisu-consented-worker-*";
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
    cpu_time_ms: Option<u64>,
    /// A reply echoing a request id has been seen. Before that, the worker
    /// may still be loading and can refuse without having read the prepare.
    correlated_reply_seen: bool,
    /// Set only once the unit is observed stopped and the wrapper reaped.
    /// Until then `Drop` retries the stop, so an `Unreaped` cancel is not
    /// forgotten.
    unit_stopped: bool,
    /// Removal must remain refused until the unit is confirmed stopped.
    consented_install: Option<VerifiedConsentedInstall>,
    manifest_file: Option<tempfile::NamedTempFile>,
    consented_lease: Option<ConsentedLease>,
}

/// Runtime factory output used by the existing Caps Lock Local path.  The
/// production Whisper adapter remains available, while an explicit private
/// development manifest selects the supervised development adapter.
pub fn refuse_if_development_worker_loaded() -> Result<(), SupervisorError> {
    refuse_if_live_units(&live_units(&[DEV_WORKER_UNITS, CONSENTED_WORKER_UNITS])?)
}

#[cfg(test)]
fn refuse_if_units_live(pattern: &str) -> Result<(), SupervisorError> {
    refuse_if_live_units(&live_units(&[pattern])?)
}

fn refuse_if_live_units(units: &[String]) -> Result<(), SupervisorError> {
    if units.is_empty() {
        Ok(())
    } else {
        Err(SupervisorError::Busy)
    }
}

pub enum RuntimeWorker {
    Whisper(WhisperCppWorker),
    Development(Box<DevWorker>),
    Consented(Box<DevWorker>),
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
            Self::Development(worker) | Self::Consented(worker) => {
                worker.exchange(control, pcm, deadline)
            }
        }
    }

    fn cancel_and_reap(&mut self) -> Result<ReapOutcome, SupervisorError> {
        match self {
            Self::Whisper(worker) => worker.cancel_and_reap(),
            Self::Development(worker) | Self::Consented(worker) => worker.cancel_and_reap(),
        }
    }

    fn last_inference_metrics(&self) -> super::supervisor::InferenceMetrics {
        match self {
            Self::Whisper(worker) => worker.last_inference_metrics(),
            Self::Development(worker) | Self::Consented(worker) => worker.last_inference_metrics(),
        }
    }
}

impl DevWorker {
    pub fn launch(
        manifest: DevManifest,
        manifest_hash: impl Into<String>,
        manifest_path: &Path,
    ) -> Result<Self, SupervisorError> {
        Self::launch_isolated(manifest, manifest_hash.into(), manifest_path, None, None)
    }

    pub(crate) fn launch_consented(
        manifest: DevManifest,
        manifest_hash: String,
        manifest_file: tempfile::NamedTempFile,
        install_root: &Path,
        install: Option<VerifiedConsentedInstall>,
        lease: ConsentedLease,
    ) -> Result<Self, SupervisorError> {
        let mut worker = Self::launch_isolated(
            manifest,
            manifest_hash,
            manifest_file.path(),
            Some(install_root),
            Some(&lease.path),
        )?;
        worker.consented_install = install;
        worker.consented_lease = Some(lease);
        worker.manifest_file = Some(manifest_file);
        Ok(worker)
    }

    fn launch_isolated(
        manifest: DevManifest,
        manifest_hash: String,
        manifest_path: &Path,
        install_root: Option<&Path>,
        lease_path: Option<&Path>,
    ) -> Result<Self, SupervisorError> {
        // Only one transient worker may hold the model/GPU at a time,
        // including one that is still starting or being stopped.
        if !Path::new(FLOCK).is_file() {
            return Err(SupervisorError::Unavailable(
                "/usr/bin/flock is required for Local worker locks",
            ));
        }
        let gpu_lock = prepare_gpu_lock()?;
        // This early probe gives a clear refusal. The transient unit's atomic
        // nonblocking flock is authoritative when two launches race this probe.
        let probe = open_gpu_lock(&gpu_lock)?;
        lock_gpu(&probe)?;
        drop(probe);
        refuse_if_development_worker_loaded()?;
        let prefix = if install_root.is_some() {
            "voisu-consented-worker"
        } else {
            "voisu-dev-worker"
        };
        let unit = format!(
            "{prefix}-{}-{}",
            std::process::id(),
            NEXT_UNIT.fetch_add(1, Ordering::Relaxed)
        );
        let cache = cache_path(&manifest);
        std::fs::create_dir_all(&cache).map_err(|_| SupervisorError::LoadAborted)?;
        let args = isolated_systemd_run_args(
            &unit,
            &cache,
            &manifest,
            manifest_path,
            install_root,
            &gpu_lock,
            lease_path,
        )?;
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
            cpu_time_ms: None,
            correlated_reply_seen: false,
            unit_stopped: false,
            consented_install: None,
            manifest_file: None,
            consented_lease: None,
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
            cpu_time_ms: None,
            correlated_reply_seen: false,
            // Nothing was launched, so there is nothing for `Drop` to stop.
            unit_stopped: true,
            consented_install: None,
            manifest_file: None,
            consented_lease: None,
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
        // Per-request worker CPU time; a reply without it must not inherit
        // the previous request's value.
        self.cpu_time_ms = value.get("cpu_time_ms").and_then(Value::as_u64);
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
        let status = value.get("status").and_then(Value::as_str);
        // A worker that refuses while loading (for example `vram_admission`)
        // writes its error before it has read the prepare, so the id is
        // empty. Only that startup refusal may be uncorrelated: it can only
        // fail the pending prepare, never satisfy it. Once any reply has
        // echoed a request id, and for every successful reply, the id must
        // match exactly.
        let startup_refusal =
            !self.correlated_reply_seen && id.is_empty() && status == Some("error");
        if id != correlation.request_id && !startup_refusal {
            return Err(SupervisorError::Protocol(FrameError::CorrelationMismatch));
        }
        if !startup_refusal {
            self.correlated_reply_seen = true;
        }
        match status {
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
        self.resident = value.get("resident").and_then(Value::as_bool) == Some(true);
    }

    /// Stops the unit within `REAP_OBSERVE + CANCEL_GRACE`.  Liveness is
    /// decided from the unit's `ActiveState` and the `systemd-run` wrapper,
    /// never from `systemctl stop`'s exit status: a `--collect` unit whose
    /// worker already exited is gone, and stopping it fails harmlessly.
    fn unit_stop(&mut self) -> ReapOutcome {
        // Closing the pipes lets a healthy worker see EOF and exit by itself.
        self.stdin.take();
        self.stdout.take();
        let observe = Instant::now() + REAP_OBSERVE;
        // A blocking `stop` waits for the unit's whole TimeoutStopSec.
        let _ = systemctl_until(&["stop", "--no-block", &self.unit], observe);
        if self.wait_stopped(observe) {
            return self.confirm_stopped(ReapOutcome::Exited);
        }
        let grace = Instant::now() + CANCEL_GRACE;
        let _ = systemctl_until(&["kill", "--signal=SIGKILL", &self.unit], grace);
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
        }
        if self.wait_stopped(grace) {
            return self.confirm_stopped(ReapOutcome::Killed);
        }
        ReapOutcome::Unreaped
    }

    fn wait_stopped(&mut self, deadline: Instant) -> bool {
        loop {
            if self.is_stopped(deadline) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn is_stopped(&mut self, deadline: Instant) -> bool {
        let wrapper_exited = self
            .child
            .as_mut()
            .is_none_or(|child| matches!(child.try_wait(), Ok(Some(_))));
        wrapper_exited
            && systemctl_until(
                &["show", "--property=ActiveState", "--value", &self.unit],
                deadline,
            )
            .is_some_and(|output| {
                output.status.success()
                    && state_is_stopped(String::from_utf8_lossy(&output.stdout).trim())
            })
    }

    fn confirm_stopped(&mut self, outcome: ReapOutcome) -> ReapOutcome {
        self.child = None;
        self.unit_stopped = true;
        outcome
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
            cpu_time_ms: self.cpu_time_ms,
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
        if let Some(pcm_bytes) = pcm_bytes
            && (pcm.map_or(0, <[u8]>::len) != pcm_bytes || pcm_bytes > MAX_PCM_BYTES)
        {
            return Err(SupervisorError::Protocol(FrameError::DeclaredPcmMismatch {
                declared: pcm_bytes,
                actual: pcm.map_or(0, <[u8]>::len),
            }));
        }
        let mut request = json!({"v": 1, "op": op, "id": id});
        if let Some(pcm_bytes) = pcm_bytes {
            request["pcm_bytes"] = Value::from(pcm_bytes);
        }
        let reply = match self.exchange_json(request, pcm, deadline) {
            Ok(reply) => reply,
            Err(error) => {
                if matches!(error, SupervisorError::Crashed) {
                    // --wait propagates the guard's conflict exit code. The
                    // unit can already be collected, so consult our wrapper.
                    let observe = deadline.min(Instant::now() + CANCEL_GRACE);
                    while let Some(child) = self.child.as_mut() {
                        match child.try_wait() {
                            Ok(Some(status)) if status.code() == Some(GPU_LOCK_CONFLICT) => {
                                return Err(SupervisorError::Unavailable(GPU_BUSY));
                            }
                            Ok(Some(_)) | Err(_) => break,
                            Ok(None) if Instant::now() < observe => {
                                std::thread::sleep(Duration::from_millis(1))
                            }
                            Ok(None) => break,
                        }
                    }
                }
                return Err(error);
            }
        };
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
        if !self.unit_stopped {
            let _ = self.unit_stop();
        }
        if !self.unit_stopped {
            // A failed reap must not release the lease or delete the manifest
            // underneath a live unit. Retain them conservatively until exit.
            if let Some(install) = self.consented_install.take() {
                std::mem::forget(install);
            }
            if let Some(lease) = self.consented_lease.take() {
                std::mem::forget(lease);
            }
            if let Some(file) = self.manifest_file.take() {
                std::mem::forget(file);
            }
        }
    }
}

/// Runs `systemctl --user` but gives up (and kills it) at `deadline`, so a
/// slow user manager cannot stretch cancellation.  Only for short outputs.
fn systemctl_until(args: &[&str], deadline: Instant) -> Option<std::process::Output> {
    let child = Command::new(SYSTEMCTL)
        .arg("--user")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child_until(child, deadline)
}

fn child_until(mut child: Child, deadline: Instant) -> Option<std::process::Output> {
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// `inactive` and `failed` units have no processes left; every other state
/// (`active`, `activating`, `deactivating`, `reloading`, ...) may still hold
/// the worker.
fn state_is_stopped(state: &str) -> bool {
    matches!(state, "inactive" | "failed")
}

fn write_until(
    writer: &mut ChildStdin,
    bytes: &[u8],
    deadline: Instant,
) -> Result<(), SupervisorError> {
    // POLLOUT only promises some free pipe space; a blocking write of a
    // larger frame would then wait on the reader past the deadline.  With
    // O_NONBLOCK the write takes what fits and returns EAGAIN otherwise.
    set_nonblocking(writer.as_raw_fd())?;
    let mut offset = 0;
    while offset < bytes.len() {
        wait_fd(writer.as_raw_fd(), libc::POLLOUT, deadline)?;
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
            if !matches!(error.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock) {
                return Err(SupervisorError::Crashed);
            }
        }
    }
    Ok(())
}

fn set_nonblocking(fd: std::os::fd::RawFd) -> Result<(), SupervisorError> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(SupervisorError::Crashed);
    }
    if flags & libc::O_NONBLOCK == 0
        && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(SupervisorError::Crashed);
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
    let mut pollfd = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    loop {
        // Recomputed per attempt so EINTR retries cannot extend the deadline.
        let timeout = deadline.saturating_duration_since(Instant::now());
        if timeout.is_zero() {
            return Err(SupervisorError::TimedOut);
        }
        let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
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
    requested == observed && matches!(observed, "cpu" | "cuda:0")
}

fn cache_path(manifest: &DevManifest) -> PathBuf {
    manifest
        .options
        .get("cache_dir")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| default_cache_path(Path::new(&manifest.model_path)))
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

/// Loaded units matching `pattern` that may still hold a worker.
fn live_units(patterns: &[&str]) -> Result<Vec<String>, SupervisorError> {
    let output = Command::new(SYSTEMCTL)
        .args([
            "--user",
            "list-units",
            "--all",
            "--type=service",
            "--plain",
            "--no-legend",
        ])
        .args(patterns)
        .output()
        .map_err(|_| SupervisorError::Unavailable("cannot list development worker units"))?;
    if !output.status.success() {
        return Err(SupervisorError::Unavailable(
            "cannot list development worker units",
        ));
    }
    Ok(parse_live_units(&String::from_utf8_lossy(&output.stdout)))
}

/// Parses `list-units --plain --no-legend` rows (`UNIT LOAD ACTIVE SUB ...`).
fn parse_live_units(listing: &str) -> Vec<String> {
    listing
        .lines()
        .filter_map(|line| {
            let mut columns = line.split_whitespace().skip_while(|column| *column == "●");
            let (name, load, active) = (columns.next()?, columns.next()?, columns.next()?);
            (load == "loaded" && !state_is_stopped(active)).then(|| name.to_owned())
        })
        .collect()
}

#[cfg(test)]
fn systemd_run_args(
    unit: &str,
    cache: &Path,
    manifest: &DevManifest,
    manifest_path: &Path,
) -> Vec<String> {
    isolated_systemd_run_args(
        unit,
        cache,
        manifest,
        manifest_path,
        None,
        Path::new("/run/user/1000/voisu/local-gpu-worker.lock"),
        None,
    )
    .unwrap()
}

fn isolated_systemd_run_args(
    unit: &str,
    cache: &Path,
    manifest: &DevManifest,
    manifest_path: &Path,
    install_root: Option<&Path>,
    gpu_lock: &Path,
    lease_path: Option<&Path>,
) -> Result<Vec<String>, SupervisorError> {
    for path in [
        Path::new(&manifest.runtime.python),
        Path::new(&manifest.runtime.worker),
        manifest_path,
    ] {
        literal_path(path)?;
    }
    // --quiet drops systemd-run's own "Running as unit", "Finished with
    // result" and resource-usage chatter; worker stderr stays inherited, and
    // a failed unit still exits non-zero.
    let mut args = vec![
        "--user".into(),
        "--quiet".into(),
        "--pipe".into(),
        "--wait".into(),
        "--collect".into(),
        format!("--unit={unit}"),
        "--property=NoNewPrivileges=yes".into(),
        "--property=PrivateNetwork=yes".into(),
        "--property=RestrictAddressFamilies=AF_UNIX".into(),
        "--property=ProtectSystem=strict".into(),
        "--property=ProtectHome=read-only".into(),
        format!("--property=ReadWritePaths={}", systemd_path(cache)?),
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
        format!("--setenv=HOME={}", literal_path(cache)?),
        format!("--setenv=TMPDIR={}", literal_path(cache)?),
        format!("--setenv=TMP={}", literal_path(cache)?),
        format!("--setenv=TEMP={}", literal_path(cache)?),
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
        args.push(format!(
            "--setenv=LD_LIBRARY_PATH={}",
            literal_path(library)?
        ));
    }
    let mut read_only = vec![systemd_path(gpu_lock)?];
    if let Some(root) = install_root {
        read_only.extend([systemd_path(root)?, systemd_path(manifest_path)?]);
    }
    if let Some(lease) = lease_path {
        read_only.push(systemd_path(lease)?);
    }
    args.push(format!("--property=ReadOnlyPaths={}", read_only.join(" ")));
    args.push("--expand-environment=no".into());
    args.push("--".into());
    args.extend(gpu_guard_args(gpu_lock, lease_path)?);
    args.extend([
        manifest.runtime.python.clone(),
        "-I".into(),
        manifest.runtime.worker.clone(),
        "--manifest".into(),
        manifest_path.to_string_lossy().into_owned(),
    ]);
    Ok(args)
}

/// `--setenv` is a literal assignment, not a unit-file quoted list.
fn literal_path(path: &Path) -> Result<&str, SupervisorError> {
    let value = path
        .to_str()
        .filter(|value| path.is_absolute() && !value.chars().any(char::is_control))
        .ok_or(SupervisorError::Unavailable(
            "worker paths must be absolute UTF-8 without control characters",
        ))?;
    Ok(value)
}

/// Path-list properties are unquoted by systemd-run; percent stays literal.
fn systemd_path(path: &Path) -> Result<String, SupervisorError> {
    let value = literal_path(path)?;
    Ok(format!(
        "\"{}\"",
        value.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

fn gpu_guard_args(gpu_lock: &Path, lease: Option<&Path>) -> Result<Vec<String>, SupervisorError> {
    // --no-fork transfers the open lock descriptors through exec, so killing
    // the launcher (or an intermediate guard) never unlocks a live worker.
    let mut args = vec![
        FLOCK.into(),
        "--nonblock".into(),
        "--exclusive".into(),
        "--conflict-exit-code=73".into(),
        "--no-fork".into(),
        literal_path(gpu_lock)?.into(),
    ];
    if let Some(lease) = lease {
        args.extend([
            FLOCK.into(),
            "--nonblock".into(),
            "--shared".into(),
            "--conflict-exit-code=74".into(),
            "--no-fork".into(),
            literal_path(lease)?.into(),
        ]);
    }
    Ok(args)
}

fn prepare_gpu_lock() -> Result<PathBuf, SupervisorError> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .ok_or(SupervisorError::Unavailable(
            "XDG_RUNTIME_DIR is required for the Local GPU lock",
        ))?;
    literal_path(&runtime)?;
    let directory = runtime.join("voisu");
    crate::local_model::ensure_private_dir(&directory).map_err(|_| {
        SupervisorError::Unavailable("cannot create private Local GPU lock directory")
    })?;
    let path = directory.join("local-gpu-worker.lock");
    let file = open_gpu_lock(&path)?;
    if file
        .metadata()
        .map_err(|_| SupervisorError::LoadAborted)?
        .permissions()
        .mode()
        & 0o777
        != 0o600
    {
        return Err(SupervisorError::Unavailable(
            "Local GPU lock must have mode 0600",
        ));
    }
    Ok(path)
}

fn open_gpu_lock(path: &Path) -> Result<File, SupervisorError> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| SupervisorError::Unavailable("cannot open Local GPU lock"))
}

fn lock_gpu(file: &File) -> Result<(), SupervisorError> {
    // SAFETY: flock borrows a valid owned descriptor and dereferences no memory.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        Ok(())
    } else if std::io::Error::last_os_error().kind() == ErrorKind::WouldBlock {
        Err(SupervisorError::Unavailable(GPU_BUSY))
    } else {
        Err(SupervisorError::Unavailable("cannot lock Local GPU"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_model::{DevLimits, DevRuntimeManifest};

    #[test]
    fn consented_path_properties_preserve_literal_special_characters() {
        for (path, encoded) in [
            ("/state/100%", "\"/state/100%\""),
            ("/state/raja's-state", "\"/state/raja's-state\""),
            ("/state/a b", "\"/state/a b\""),
            ("/state/a\"b", "\"/state/a\\\"b\""),
            ("/state/a\\b", "\"/state/a\\\\b\""),
        ] {
            assert_eq!(systemd_path(Path::new(path)).unwrap(), encoded);
        }
    }

    #[test]
    fn consented_paths_reject_control_characters_and_environment_stays_literal() {
        for value in ["/state/a\nb", "/state/a\rb", "/state/a\tb", "/state/a\0b"] {
            let mut manifest = test_manifest("cuda:0");
            manifest.options.insert("cache_dir".into(), json!(value));
            let error = isolated_systemd_run_args(
                "test",
                Path::new(value),
                &manifest,
                Path::new("/state/manifest"),
                None,
                Path::new("/run/gpu"),
                None,
            )
            .unwrap_err();
            assert!(error.to_string().contains("control characters"));
            assert!(systemd_path(Path::new(value)).is_err());
        }
        let mut manifest = test_manifest("cuda:0");
        let cache = Path::new("/state/a b%c'd\"e\\f");
        manifest
            .options
            .insert("library".into(), json!(cache.join("lib.so")));
        let args = systemd_run_args("test", cache, &manifest, Path::new("/state/manifest"));
        for name in ["HOME", "TMPDIR", "TMP", "TEMP", "LD_LIBRARY_PATH"] {
            assert!(args.contains(&format!("--setenv={name}=/state/a b%c'd\"e\\f")));
        }
        assert!(args.contains(&"--property=ReadWritePaths=\"/state/a b%c'd\\\"e\\\\f\"".into()));
    }

    #[test]
    fn consented_guard_conflict_is_a_clear_bounded_refusal() {
        let state = tempfile::tempdir().unwrap();
        let gpu = state.path().join("gpu.lock");
        let held = open_gpu_lock(&gpu).unwrap();
        lock_gpu(&held).unwrap();
        let mut args = gpu_guard_args(&gpu, None).unwrap();
        let executable = args.remove(0);
        let mut child = Command::new(executable)
            .args(args)
            .arg("/bin/true")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut worker = DevWorker::for_test(test_manifest("cuda:0"), "h");
        worker.stdin = child.stdin.take();
        worker.stdout = child.stdout.take();
        worker.child = Some(child);
        let started = Instant::now();
        assert_eq!(
            worker.exchange(
                ControlFrame::Prepare(corr()),
                None,
                started + Duration::from_secs(3)
            ),
            Err(SupervisorError::Unavailable(GPU_BUSY))
        );
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(
            worker.child.as_mut().unwrap().wait().unwrap().code(),
            Some(73)
        );
    }

    #[test]
    fn consented_unit_guards_exclude_both_prefixes_and_outlive_launcher_lease() {
        use std::io::{BufRead, BufReader, Write};
        let state = tempfile::tempdir().unwrap();
        let gpu = state.path().join("gpu.lock");
        drop(open_gpu_lock(&gpu).unwrap());
        let store = crate::local_model::ModelStore::open(state.path().join("models")).unwrap();
        crate::local_model::consented::install_tests::install(&store);
        let entry = crate::local_model::consented::install_tests::entry();
        let install = crate::local_model::consented::verify_with_entry(&store, &entry).unwrap();
        let lease =
            crate::local_model::consented::consented_install_lease(install.layout()).unwrap();
        let mut args = gpu_guard_args(&gpu, Some(&lease.path)).unwrap();
        let executable = args.remove(0);
        struct Guard(Child);
        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut worker = Guard(
            Command::new(executable)
                .args(args)
                .arg("/bin/cat")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        worker
            .0
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"locks acquired\n")
            .unwrap();
        let mut line = String::new();
        BufReader::new(worker.0.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(line, "locks acquired\n"); // Proves both exec guards completed.
        drop(install);
        let lease_path = lease.path.clone();
        drop(lease); // Simulates the launcher dying: only unit-owned fds remain.
        assert_eq!(
            lock_gpu(&open_gpu_lock(&gpu).unwrap()),
            Err(SupervisorError::Unavailable(GPU_BUSY))
        );
        for candidate_lease in [None, Some(lease_path.as_path())] {
            let mut args = gpu_guard_args(&gpu, candidate_lease).unwrap();
            let executable = args.remove(0);
            let status = Command::new(executable)
                .args(args)
                .arg("/bin/true")
                .status()
                .unwrap();
            assert_eq!(status.code(), Some(GPU_LOCK_CONFLICT));
        }
        let mut lease_check = Command::new(FLOCK);
        let status = lease_check
            .args(["--nonblock", "--exclusive", "--conflict-exit-code=74"])
            .arg(&lease_path)
            .arg("/bin/true")
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(74));
        drop(worker);
        assert!(lease_check.status().unwrap().success());
        assert!(lock_gpu(&open_gpu_lock(&gpu).unwrap()).is_ok());
    }

    #[test]
    fn transient_worker_unit_silences_systemd_run_chatter_but_still_waits() {
        let args = systemd_run_args(
            "voisu-dev-worker-1-1",
            Path::new("/state/cache"),
            &test_manifest("cuda:0"),
            Path::new("/state/manifest.json"),
        );
        assert_eq!(
            args[..5],
            ["--user", "--quiet", "--pipe", "--wait", "--collect"]
        );
    }

    #[test]
    fn consented_transient_unit_preserves_the_offline_sandbox() {
        let mut manifest = test_manifest("cuda:0");
        manifest.runtime.python = "/state/install/runtime/venv/bin/python".into();
        manifest.runtime.worker = "/state/install/worker/worker.py".into();
        assert_eq!(
            isolated_systemd_run_args(
                "voisu-consented-worker-123-1",
                Path::new("/state/cache"),
                &manifest,
                Path::new("/state/manifest.json"),
                Some(Path::new("/state/install")),
                Path::new("/run/user/1000/voisu/local-gpu-worker.lock"),
                Some(Path::new("/state/models/private/hash.lease")),
            )
            .unwrap(),
            vec![
                "--user",
                "--quiet",
                "--pipe",
                "--wait",
                "--collect",
                "--unit=voisu-consented-worker-123-1",
                "--property=NoNewPrivileges=yes",
                "--property=PrivateNetwork=yes",
                "--property=RestrictAddressFamilies=AF_UNIX",
                "--property=ProtectSystem=strict",
                "--property=ProtectHome=read-only",
                "--property=ReadWritePaths=\"/state/cache\"",
                "--property=MemoryMax=1024",
                "--property=TasksMax=4",
                "--property=CPUQuota=100%",
                "--property=RestrictSUIDSGID=yes",
                "--property=UMask=0077",
                "--setenv=PYTHONNOUSERSITE=1",
                "--setenv=HF_HUB_OFFLINE=1",
                "--setenv=TRANSFORMERS_OFFLINE=1",
                "--setenv=HF_DATASETS_OFFLINE=1",
                "--setenv=HOME=/state/cache",
                "--setenv=TMPDIR=/state/cache",
                "--setenv=TMP=/state/cache",
                "--setenv=TEMP=/state/cache",
                "--setenv=VOISU_DEV_DEVICE=cuda:0",
                "--setenv=CUDA_VISIBLE_DEVICES=0",
                "--setenv=CUDA_DEVICE_ORDER=PCI_BUS_ID",
                "--property=ReadOnlyPaths=\"/run/user/1000/voisu/local-gpu-worker.lock\" \"/state/install\" \"/state/manifest.json\" \"/state/models/private/hash.lease\"",
                "--expand-environment=no",
                "--",
                FLOCK,
                "--nonblock",
                "--exclusive",
                "--conflict-exit-code=73",
                "--no-fork",
                "/run/user/1000/voisu/local-gpu-worker.lock",
                FLOCK,
                "--nonblock",
                "--shared",
                "--conflict-exit-code=74",
                "--no-fork",
                "/state/models/private/hash.lease",
                "/state/install/runtime/venv/bin/python",
                "-I",
                "/state/install/worker/worker.py",
                "--manifest",
                "/state/manifest.json",
            ]
        );
    }

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
    fn uncorrelated_startup_refusal_keeps_the_worker_code_and_message() {
        // The Python worker writes this before reading any request when the
        // adapter refuses to load, so its id cannot echo the prepare id.
        let mut worker = DevWorker::for_test(test_manifest("cuda:0"), "h");
        let frame = worker
            .map_reply(
                json!({
                    "v": 1,
                    "id": "",
                    "status": "error",
                    "code": "vram_admission",
                    "message": "Qwen peak 6120538112 does not fit VRAM admission"
                }),
                &corr(),
            )
            .unwrap();
        let WorkerFrame::Error { code, metadata } = frame else {
            panic!("expected an error frame");
        };
        assert_eq!(code, "vram_admission");
        assert_eq!(
            metadata.get("message").and_then(Value::as_str),
            Some("Qwen peak 6120538112 does not fit VRAM admission")
        );
    }

    #[test]
    fn uncorrelated_error_after_a_correlated_reply_is_a_mismatch() {
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
        assert_eq!(
            worker
                .map_reply(
                    json!({"id": "", "status": "error", "code": "invalid_id", "message": "m"}),
                    &corr(),
                )
                .unwrap_err(),
            SupervisorError::Protocol(FrameError::CorrelationMismatch)
        );
    }

    #[test]
    fn only_errors_may_be_uncorrelated() {
        let mut worker = DevWorker::for_test(test_manifest("cpu"), "h");
        for reply in [
            json!({
                "id": "",
                "status": "ready",
                "model_id": "nemotron-3.5-0.6b",
                "observed_device": "cpu",
                "resident": false
            }),
            json!({"id": "", "status": "transcript", "text": "hi", "observed_device": "cpu"}),
            json!({"id": "", "status": "no_text", "observed_device": "cpu"}),
            json!({"id": "other", "status": "error", "code": "x", "message": "y"}),
        ] {
            assert_eq!(
                worker.map_reply(reply, &corr()).unwrap_err(),
                SupervisorError::Protocol(FrameError::CorrelationMismatch)
            );
        }
    }

    #[test]
    fn transcript_records_worker_cpu_time() {
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
        worker
            .map_reply(
                json!({
                    "id": "q",
                    "status": "transcript",
                    "text": "hi",
                    "observed_device": "cpu",
                    "resident": true,
                    "inference_ms": 40,
                    "cpu_time_ms": 75,
                    "peak_host_ram_bytes": 10,
                    "peak_gpu_memory_bytes": 0
                }),
                &corr(),
            )
            .unwrap();
        let metrics = worker.last_inference_metrics();
        assert_eq!(metrics.cpu_time_ms, Some(75));
        assert_eq!(metrics.inference_ms, Some(40));
        assert_eq!(metrics.peak_host_ram_bytes, Some(10));
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

    #[test]
    fn write_until_times_out_on_a_stalled_reader() {
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let frame = vec![0u8; MAX_PCM_BYTES];
        let started = Instant::now();
        let result = write_until(&mut stdin, &frame, started + Duration::from_millis(200));
        let elapsed = started.elapsed();
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(result, Err(SupervisorError::TimedOut));
        assert!(elapsed < Duration::from_secs(1), "took {elapsed:?}");
    }

    #[test]
    fn cancellation_command_deadline_kills_a_stalled_child() {
        let child = Command::new("sleep")
            .arg("30")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let started = Instant::now();
        assert!(child_until(child, started + Duration::from_millis(100)).is_none());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn write_until_delivers_every_byte_to_a_healthy_reader() {
        let mut child = Command::new("wc")
            .arg("-c")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let frame: Vec<u8> = (0..MAX_PCM_BYTES).map(|i| i as u8).collect();
        write_until(&mut stdin, &frame, Instant::now() + Duration::from_secs(30)).unwrap();
        drop(stdin);
        let output = child.wait_with_output().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            MAX_PCM_BYTES.to_string()
        );
    }

    #[test]
    fn live_units_ignore_stopped_and_unloaded_rows() {
        let listing = "\
voisu-dev-worker-1-1.service loaded active running [systemd-run] python
voisu-dev-worker-1-2.service loaded activating start [systemd-run] python
voisu-dev-worker-1-3.service loaded deactivating stop-sigterm [systemd-run] python
voisu-dev-worker-1-4.service loaded inactive dead [systemd-run] python
\u{25cf} voisu-dev-worker-1-5.service loaded failed failed [systemd-run] python
voisu-dev-worker-1-6.service not-found inactive dead voisu-dev-worker-1-6.service
";
        let live = parse_live_units(listing);
        assert_eq!(
            live,
            [
                "voisu-dev-worker-1-1.service",
                "voisu-dev-worker-1-2.service",
                "voisu-dev-worker-1-3.service",
            ]
        );
        assert_eq!(refuse_if_live_units(&live), Err(SupervisorError::Busy));
        assert_eq!(refuse_if_live_units(&[]), Ok(()));
    }

    /// Transient user unit owned by one test.  Its names never match
    /// `voisu-dev-worker-*`, so the live development service is unaffected.
    struct TestUnit(String);

    impl TestUnit {
        fn name(tag: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(1);
            Self(format!(
                "voisu-test-{tag}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ))
        }

        /// With `wait`, returns the running `systemd-run --wait` wrapper, as
        /// `launch` does; otherwise waits for the unit to be started.
        fn run(&self, wait: bool, extra: &[&str], command: &[&str]) -> Option<Child> {
            let mut run = Command::new(SYSTEMD_RUN);
            run.args(["--user", "--quiet", "--collect"])
                .args(wait.then_some("--wait"))
                .arg(format!("--unit={}", self.0))
                .args(extra)
                .arg("--")
                .args(command)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            if wait {
                return Some(run.spawn().unwrap());
            }
            assert!(run.status().unwrap().success());
            None
        }

        fn state(&self) -> String {
            let output = Command::new(SYSTEMCTL)
                .args([
                    "--user",
                    "show",
                    "--property=ActiveState",
                    "--value",
                    &self.0,
                ])
                .output()
                .unwrap();
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        }

        fn wait_for_state(&self, state: &str) -> bool {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if self.state() == state {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            false
        }

        fn worker(&self, child: Option<Child>) -> DevWorker {
            let mut worker = DevWorker::for_test(test_manifest("cpu"), "h");
            worker.unit = self.0.clone();
            worker.child = child;
            worker.unit_stopped = false;
            worker
        }
    }

    impl Drop for TestUnit {
        fn drop(&mut self) {
            for args in [
                ["kill", "--signal=SIGKILL", self.0.as_str()],
                ["stop", "--no-block", self.0.as_str()],
            ] {
                let _ = Command::new(SYSTEMCTL)
                    .arg("--user")
                    .args(args)
                    .stderr(Stdio::null())
                    .status();
            }
            let _ = Command::new(SYSTEMCTL)
                .args(["--user", "reset-failed", &self.0])
                .stderr(Stdio::null())
                .status();
        }
    }

    fn user_manager_available() -> bool {
        let available = Command::new(SYSTEMCTL)
            .args(["--user", "show-environment"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if !available {
            eprintln!("skipping: no systemd user manager");
        }
        available
    }

    #[test]
    fn cancel_is_bounded_when_the_unit_ignores_its_stop_signal() {
        if !user_manager_available() {
            return;
        }
        let unit = TestUnit::name("slow-stop");
        // SIGCONT as the stop signal makes `systemctl stop` wait the full
        // 90 s TimeoutStopSec, like a worker stuck in a CUDA call.
        let wrapper = unit
            .run(
                true,
                &[
                    "--property=KillSignal=SIGCONT",
                    "--property=TimeoutStopSec=90",
                ],
                &["/bin/sleep", "60"],
            )
            .unwrap();
        assert!(unit.wait_for_state("active"));
        let mut worker = unit.worker(Some(wrapper));
        let started = Instant::now();
        let outcome = worker.unit_stop();
        let elapsed = started.elapsed();
        assert_eq!(outcome, ReapOutcome::Killed);
        assert!(
            elapsed < REAP_OBSERVE + CANCEL_GRACE + Duration::from_secs(1),
            "took {elapsed:?}"
        );
        assert!(state_is_stopped(&unit.state()));
        assert!(worker.unit_stopped);
    }

    #[test]
    fn cancel_after_the_collected_unit_exited_reports_exited() {
        if !user_manager_available() {
            return;
        }
        let unit = TestUnit::name("gone");
        let mut wrapper = unit.run(true, &[], &["/bin/true"]).unwrap();
        wrapper.wait().unwrap();
        let mut worker = unit.worker(Some(wrapper));
        assert_eq!(worker.unit_stop(), ReapOutcome::Exited);
        assert!(worker.unit_stopped);
    }

    #[test]
    fn drop_stops_a_unit_that_was_never_confirmed_stopped() {
        if !user_manager_available() {
            return;
        }
        let unit = TestUnit::name("drop");
        assert!(unit.run(false, &[], &["/bin/sleep", "60"]).is_none());
        assert!(unit.wait_for_state("active"));
        // No wrapper left, as after an earlier cancel that took it.
        drop(unit.worker(None));
        assert!(state_is_stopped(&unit.state()));
    }

    #[test]
    fn busy_while_a_matching_unit_is_still_activating() {
        if !user_manager_available() {
            return;
        }
        let unit = TestUnit::name("busy");
        let pattern = format!("{}*", unit.0);
        assert_eq!(refuse_if_units_live(&pattern), Ok(()));
        assert!(
            unit.run(
                false,
                &["--no-block", "--property=Type=oneshot"],
                &["/bin/sleep", "60"],
            )
            .is_none()
        );
        assert!(unit.wait_for_state("activating"));
        assert_eq!(refuse_if_units_live(&pattern), Err(SupervisorError::Busy));
        let mut worker = unit.worker(None);
        assert_ne!(worker.unit_stop(), ReapOutcome::Unreaped);
        assert_eq!(refuse_if_units_live(&pattern), Ok(()));
    }
}
