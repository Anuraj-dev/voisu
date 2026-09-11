//! Non-admitted Pilot Candidate worker using process-wrapped `whisper-cli`.
//!
//! One native executable, one verified model file, no Python/CUDA/FFI/JIT.
//! CPU-only, absolute paths, scrubbed environment, no downloader, no shell,
//! no caller-supplied paths. The worker is persistent across Recordings:
//! `Prepare` verifies the artifact and runs first-inference health through the
//! exact inference path, and repeated `Transcribe` calls reuse that proof
//! without reloading or re-verifying. Metadata checks alone never yield
//! `Ready`. PCM travels in a sealed memory-only file descriptor passed as
//! `/proc/self/fd/N`; no named temporary WAV ever touches the filesystem.
//! Each exchange spawns at most one child in its own process group, drains
//! bounded stdout/stderr concurrently so a noisy child cannot wedge the pipe,
//! kills the owned group past the deadline, and reaps inline with bounded
//! reader joins so a child-held descriptor fails closed instead of hanging
//! the daemon.

use std::io::{Read, Seek, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::bounds::{
    CANCEL_GRACE, MAX_PCM_BYTES, MAX_RETAINED_STDERR_BYTES, PCM_CHANNELS, PCM_SAMPLE_RATE_HZ,
    REAP_OBSERVE,
};
use super::protocol::{
    ControlFrame, Correlation, FrameError, WorkerFrame, is_silence_pcm, validate_pcm,
    validate_transcript,
};
use super::runtime::reject_forbidden_program;
use super::supervisor::{ReapOutcome, SupervisorError, WorkerChild};

/// Fixed executable for the Pilot Candidate worker.
pub const PILOT_WHISPER_CLI: &str = "/usr/bin/whisper-cli";

/// Stdout cap: parsed JSON for a bounded transcript plus upstream envelope.
const MAX_CLI_STDOUT_BYTES: usize = 2 * 1024 * 1024;
/// Stderr is drained (never parsed as protocol) and mostly discarded; the
/// retained prefix follows the worker stderr bound.
const MAX_CLI_STDERR_BYTES: usize = MAX_RETAINED_STDERR_BYTES;
const POLL_TICK: Duration = Duration::from_millis(5);

#[derive(Clone, Debug)]
pub struct WhisperCppWorker {
    binary: PathBuf,
    model: PathBuf,
    generation: u64,
    model_receipt_hash: String,
    /// Set only by a successful first-inference health run in `Prepare`.
    /// `Transcribe` refuses work until then, so `Ready` always means the
    /// exact verified model loaded and inference succeeded — never a
    /// metadata check. Stays set across Recordings: no per-Recording reload.
    prepared: bool,
    /// Successful health loads. Stays 1 across repeated inference.
    health_loads: u32,
}

impl WhisperCppWorker {
    fn build(
        binary: PathBuf,
        model: PathBuf,
        generation: u64,
        model_receipt_hash: String,
    ) -> Result<Self, SupervisorError> {
        reject_forbidden_program(&binary).map_err(SupervisorError::Runtime)?;
        if !binary.is_absolute() || !model.is_absolute() {
            return Err(SupervisorError::Unavailable(
                "worker paths must be absolute",
            ));
        }
        Ok(Self {
            binary,
            model,
            generation,
            model_receipt_hash,
            prepared: false,
            health_loads: 0,
        })
    }

    /// Typed executable seam for unit tests. Production always uses the fixed
    /// Pilot Candidate executable and a catalog-derived model name.
    #[cfg(test)]
    pub(crate) fn new(
        binary: PathBuf,
        model: PathBuf,
        generation: u64,
        model_receipt_hash: String,
    ) -> Result<Self, SupervisorError> {
        Self::build(binary, model, generation, model_receipt_hash)
    }

    /// Bind to a verified artifact directory: the weights file comes from the
    /// catalog entry, never from the environment or the caller.
    pub(crate) fn from_artifact(
        artifact_dir: &Path,
        weights_name: &str,
        receipt_hash: &str,
    ) -> Result<Self, SupervisorError> {
        if weights_name.is_empty() || weights_name.contains('/') || weights_name.contains('\0') {
            return Err(SupervisorError::Unavailable("unsafe weights name"));
        }
        Self::build(
            PathBuf::from(PILOT_WHISPER_CLI),
            artifact_dir.join(weights_name),
            1,
            receipt_hash.to_owned(),
        )
    }

    fn check_correlation(&self, correlation: &Correlation) -> Result<(), SupervisorError> {
        if correlation.generation != self.generation
            || correlation.model_receipt_hash != self.model_receipt_hash
        {
            return Err(SupervisorError::Protocol(FrameError::CorrelationMismatch));
        }
        Ok(())
    }

    fn check_deadline(deadline: Instant) -> Result<(), SupervisorError> {
        if Instant::now() >= deadline {
            return Err(SupervisorError::TimedOut);
        }
        Ok(())
    }

    fn prepare_inner(
        &mut self,
        correlation: &Correlation,
        deadline: Instant,
    ) -> Result<WorkerFrame, SupervisorError> {
        Self::check_deadline(deadline)?;
        self.check_correlation(correlation)?;
        reject_forbidden_program(&self.binary).map_err(SupervisorError::Runtime)?;
        if !self.binary.is_file() {
            return Err(SupervisorError::LoadAborted);
        }
        match self.model.metadata() {
            Ok(meta) if meta.is_file() && meta.len() > 0 => {}
            _ => return Err(SupervisorError::LoadAborted),
        }
        // First-inference health through the exact inference path: a tiny
        // silence WAV delivered over the same sealed memfd mechanism as real
        // PCM. Exit 0 plus a parseable envelope proves the verified model
        // loaded and inference ran; anything else fails closed and `Ready`
        // is never returned from the metadata checks above alone. Byte
        // verification of the weights lives at install/receipt time; Prepare
        // pins the artifact-bound path plus the receipt hash in `check_correlation`.
        let health = wav_bytes(&[0u8; HEALTH_PCM_BYTES])
            .ok_or(SupervisorError::Unavailable("worker cache unavailable"))?;
        let (memfd, _) = sealed_memfd_wav(&health)
            .ok_or(SupervisorError::Unavailable("worker cache unavailable"))?;
        let stdout = self.run_cli(&memfd, deadline)?;
        match parse_whisper_json(&stdout) {
            Ok(_) => {
                self.prepared = true;
                self.health_loads = self.health_loads.saturating_add(1);
                Ok(WorkerFrame::Ready {
                    correlation: correlation.clone(),
                    observed_device: "cpu".into(),
                })
            }
            Err(error) => Err(error),
        }
    }

    fn transcribe_inner(
        &mut self,
        correlation: &Correlation,
        pcm: &[u8],
        deadline: Instant,
    ) -> Result<WorkerFrame, SupervisorError> {
        Self::check_deadline(deadline)?;
        self.check_correlation(correlation)?;
        if !self.prepared {
            return Err(SupervisorError::Unavailable("worker not prepared"));
        }
        validate_pcm(pcm, None).map_err(SupervisorError::Protocol)?;
        if is_silence_pcm(pcm) {
            return Ok(WorkerFrame::NoText {
                correlation: correlation.clone(),
                reason: "silence".into(),
            });
        }
        let wav = wav_bytes(pcm).ok_or(SupervisorError::Protocol(FrameError::PcmTooLarge {
            bytes: pcm.len(),
        }))?;
        let (memfd, _) = sealed_memfd_wav(&wav)
            .ok_or(SupervisorError::Unavailable("worker cache unavailable"))?;
        let stdout = self.run_cli(&memfd, deadline)?;
        match parse_whisper_json(&stdout) {
            Ok(WhisperText::Transcript(text)) => Ok(WorkerFrame::Transcript {
                correlation: correlation.clone(),
                text,
            }),
            Ok(WhisperText::NoText) => Ok(WorkerFrame::NoText {
                correlation: correlation.clone(),
                reason: "no_text".into(),
            }),
            Err(error) => Err(error),
        }
    }

    fn run_cli(&self, wav_memfd: &OwnedFd, deadline: Instant) -> Result<Vec<u8>, SupervisorError> {
        // The descriptor borrow outlives the exchange: the WAV bytes live
        // only in this memfd, opened once by the child via /proc/self/fd.
        let wav_path = format!("/proc/self/fd/{}", wav_memfd.as_raw_fd());
        let mut command = Command::new(&self.binary);
        command
            .args([
                "--model",
                &self.model.to_string_lossy(),
                "--file",
                &wav_path,
                "--language",
                "en",
                "--no-gpu",
                "--no-prints",
                "--output-json",
                "--output-file",
                "-",
            ])
            // R3: no credentials, proxy, preload, or session-bus reach the child.
            // PATH is dropped too: the binary path is absolute.
            .env_clear()
            .env("LANG", "C")
            // Own process group so deadline cleanup reaps exactly the owned
            // child tree (direct kill plus group kill) and never a daemon
            // sibling. whisper-cli spawns no grandchildren; the group kill is
            // the backstop if a future runtime does.
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // No kill_on_drop on this toolchain: the poll loop below kills past
        // the deadline and every return path joins or reaps the child, so no
        // child outlives the exchange except via a host kill -9 of ourselves.
        let mut child = command.spawn().map_err(|_| SupervisorError::LoadAborted)?;
        let pgid = child.id() as libc::pid_t;
        let stdout_rx = child
            .stdout
            .take()
            .map(|pipe| spawn_drain(pipe, MAX_CLI_STDOUT_BYTES));
        let stderr_rx = child
            .stderr
            .take()
            .map(|pipe| spawn_drain(pipe, MAX_CLI_STDERR_BYTES));
        loop {
            match child.try_wait().map_err(|_| SupervisorError::Crashed)? {
                Some(status) => {
                    // A descendant holding a pipe open must not hang the
                    // daemon: bound the reader joins and fail closed on
                    // partial output instead of delivering it.
                    let Some((stdout, _)) = await_drain(stdout_rx, REAP_OBSERVE) else {
                        return Err(SupervisorError::TimedOut);
                    };
                    let Some((stderr, _)) = await_drain(stderr_rx, REAP_OBSERVE) else {
                        return Err(SupervisorError::TimedOut);
                    };
                    return self.interpret_exit(status.code(), &stdout, &stderr);
                }
                None => {
                    if Instant::now() >= deadline {
                        // Covers a stalled child for any reason, including
                        // SIGSTOP suspension: SIGKILL applies to stopped
                        // processes, then the group backstop, then reap.
                        kill_owned_tree(&mut child, pgid);
                        let _ = await_drain(stdout_rx, CANCEL_GRACE);
                        let _ = await_drain(stderr_rx, CANCEL_GRACE);
                        return Err(SupervisorError::TimedOut);
                    }
                    std::thread::sleep(POLL_TICK);
                }
            }
        }
    }

    fn interpret_exit(
        &self,
        code: Option<i32>,
        stdout: &[u8],
        stderr: &[u8],
    ) -> Result<Vec<u8>, SupervisorError> {
        let diagnostic = String::from_utf8_lossy(&stderr[..stderr.len().min(1024)]);
        let lowered = diagnostic.to_ascii_lowercase();
        if lowered.contains("out of memory")
            || lowered.contains("bad_alloc")
            || lowered.contains("cannot allocate")
        {
            return Err(SupervisorError::OutOfMemory);
        }
        // Our own deadline kills return `TimedOut` before exit
        // interpretation, so 137 here means an external SIGKILL — almost
        // always the host OOM killer — never a transcript.
        if code == Some(137) {
            return Err(SupervisorError::OutOfMemory);
        }
        if code == Some(0) {
            return Ok(stdout.to_vec());
        }
        // Model/runtime failed to load or read its inputs: fail closed as
        // unavailable, never as a transcript. Anything else is a native crash.
        if lowered.contains("failed to load")
            || lowered.contains("could not load")
            || lowered.contains("cannot open")
            || lowered.contains("no such file")
        {
            return Err(SupervisorError::LoadAborted);
        }
        Err(SupervisorError::Crashed)
    }
}

impl WorkerChild for WhisperCppWorker {
    fn exchange(
        &mut self,
        control: ControlFrame,
        pcm: Option<&[u8]>,
        deadline: Instant,
    ) -> Result<WorkerFrame, SupervisorError> {
        match control {
            ControlFrame::Prepare(correlation) => self.prepare_inner(&correlation, deadline),
            ControlFrame::Transcribe {
                correlation,
                pcm_bytes,
            } => {
                let pcm = pcm.unwrap_or_default();
                if pcm.len() != pcm_bytes {
                    return Err(SupervisorError::Protocol(FrameError::DeclaredPcmMismatch {
                        declared: pcm_bytes,
                        actual: pcm.len(),
                    }));
                }
                self.transcribe_inner(&correlation, pcm, deadline)
            }
            ControlFrame::Cancel { .. } => Ok(WorkerFrame::Error {
                code: "cancelled".into(),
                metadata: serde_json::Map::new(),
            }),
        }
    }

    fn cancel_and_reap(&mut self) -> Result<ReapOutcome, SupervisorError> {
        // Every exchange spawns at most one child and reaps it inline
        // (including the deadline kill path), so no child outlives a call.
        Ok(ReapOutcome::Exited)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum WhisperText {
    Transcript(String),
    NoText,
}

/// Parse `--output-json` stdout: concatenate segment texts in order. A
/// successful run with zero or whitespace-only segments is `NoText`, never a
/// successful empty transcript. Malformed envelopes fail closed as a native
/// crash: the runtime broke its documented contract.
fn parse_whisper_json(stdout: &[u8]) -> Result<WhisperText, SupervisorError> {
    let value: serde_json::Value =
        serde_json::from_slice(stdout).map_err(|_| SupervisorError::Crashed)?;
    let segments = value
        .get("transcription")
        .and_then(serde_json::Value::as_array)
        .ok_or(SupervisorError::Crashed)?;
    let mut text = String::new();
    for segment in segments {
        let piece = segment
            .get("text")
            .and_then(serde_json::Value::as_str)
            .ok_or(SupervisorError::Crashed)?;
        text.push_str(piece);
    }
    let trimmed = text.trim().to_owned();
    if trimmed.is_empty() {
        return Ok(WhisperText::NoText);
    }
    validate_transcript(&trimmed).map_err(|error| match error {
        FrameError::TranscriptTooLarge { .. } | FrameError::EmbeddedNul => SupervisorError::Crashed,
        _ => SupervisorError::Protocol(error),
    })?;
    Ok(WhisperText::Transcript(trimmed))
}

/// Canonical RIFF/WAV for the bounded s16le mono 16 kHz contract. Returns
/// `None` instead of allocating past the PCM cap.
fn wav_bytes(pcm: &[u8]) -> Option<Vec<u8>> {
    if pcm.len() > MAX_PCM_BYTES || !pcm.len().is_multiple_of(2) {
        return None;
    }
    let data_len = pcm.len() as u32;
    let mut out = Vec::with_capacity(44 + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36u32.saturating_add(data_len)).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&PCM_CHANNELS.to_le_bytes());
    out.extend_from_slice(&PCM_SAMPLE_RATE_HZ.to_le_bytes());
    out.extend_from_slice(&(PCM_SAMPLE_RATE_HZ * u32::from(PCM_CHANNELS) * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(pcm);
    Some(out)
}

fn read_capped(pipe: &mut impl Read, cap: usize) -> (Vec<u8>, bool) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut truncated = false;
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() + n > cap {
                    truncated = true;
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(_) => break,
        }
    }
    // Drain the rest so the child never blocks on a full pipe.
    let mut sink = [0u8; 8192];
    while pipe.read(&mut sink).is_ok_and(|n| n > 0) {}
    (buf, truncated)
}

/// 0.25 s of silence: small enough for a fast health run, real enough to
/// travel the exact memfd inference path.
const HEALTH_PCM_BYTES: usize = 8_000;

/// Memory-only WAV delivery. The bytes live in a sealed memfd with no
/// filesystem name; the child opens it once via `/proc/self/fd/N`. Returns
/// the held descriptor (kept open until the child is reaped) plus the path
/// argument. `None` fails closed — there is no named-tempfile fallback.
fn sealed_memfd_wav(wav: &[u8]) -> Option<(OwnedFd, String)> {
    // SAFETY: memfd_create with a static name; -1 is checked. Flags carry
    // MFD_ALLOW_SEALING only, so the descriptor stays inheritable across the
    // child exec and the path below resolves for the child.
    let fd = unsafe {
        libc::memfd_create(
            c"voisu-wav".as_ptr(),
            libc::MFD_ALLOW_SEALING as libc::c_uint,
        )
    };
    if fd < 0 {
        return None;
    }
    // SAFETY: fd is a fresh owned descriptor from memfd_create.
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    if file.write_all(wav).is_err() {
        return None;
    }
    if file.seek(std::io::SeekFrom::Start(0)).is_err() {
        return None;
    }
    // Best-effort hardening: the child only ever reads.
    unsafe {
        libc::fcntl(
            file.as_raw_fd(),
            libc::F_ADD_SEALS,
            libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE,
        );
    }
    let owned: OwnedFd = file.into();
    let path = format!("/proc/self/fd/{}", owned.as_raw_fd());
    Some((owned, path))
}

fn spawn_drain<R: Read + Send + 'static>(pipe: R, cap: usize) -> mpsc::Receiver<(Vec<u8>, bool)> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut pipe = pipe;
        let _ = tx.send(read_capped(&mut pipe, cap));
    });
    rx
}

fn await_drain(
    rx: Option<mpsc::Receiver<(Vec<u8>, bool)>>,
    grace: Duration,
) -> Option<(Vec<u8>, bool)> {
    rx.and_then(|rx| rx.recv_timeout(grace).ok())
}

/// Reap exactly the owned child tree: direct kill, group backstop for any
/// descendant holding a descriptor, then wait. Best-effort signals are
/// ignored — the subsequent `wait` is what reaps.
fn kill_owned_tree(child: &mut std::process::Child, pgid: libc::pid_t) {
    let _ = child.kill();
    // SAFETY: pgid is our own child's group (spawned with process_group(0)),
    // so the signal cannot reach daemon siblings.
    unsafe {
        libc::killpg(pgid, libc::SIGKILL);
    }
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn correlation(hash: &str) -> Correlation {
        Correlation {
            daemon_nonce: "l4".into(),
            generation: 1,
            request_id: "q".into(),
            recording_id: "rec-1".into(),
            model_receipt_hash: hash.into(),
        }
    }

    fn worker_with(binary: &str) -> WhisperCppWorker {
        WhisperCppWorker::new(
            PathBuf::from(binary),
            PathBuf::from("/models/ggml-base.en.bin"),
            1,
            "hash".into(),
        )
        .unwrap()
    }

    #[test]
    fn relative_and_forbidden_programs_are_rejected_before_spawn() {
        assert!(
            WhisperCppWorker::new(
                PathBuf::from("whisper-cli"),
                PathBuf::from("/models/m.bin"),
                1,
                "h".into(),
            )
            .is_err()
        );
        assert!(
            WhisperCppWorker::new(
                PathBuf::from("/usr/bin/ollama"),
                PathBuf::from("/models/m.bin"),
                1,
                "h".into(),
            )
            .is_err()
        );
        assert!(
            WhisperCppWorker::new(
                PathBuf::from("/usr/bin/whisper-cli"),
                PathBuf::from("relative/m.bin"),
                1,
                "h".into(),
            )
            .is_err()
        );
    }

    #[test]
    fn prepare_fails_closed_without_binary_or_model() {
        let mut worker = worker_with("/nonexistent/whisper-cli");
        let error = worker
            .exchange(
                ControlFrame::Prepare(correlation("hash")),
                None,
                Instant::now() + Duration::from_secs(5),
            )
            .unwrap_err();
        assert!(matches!(error, SupervisorError::LoadAborted));
    }

    #[test]
    fn prepare_rejects_stale_correlation() {
        let mut worker = worker_with("/bin/true");
        let mut bad = correlation("hash");
        bad.generation = 99;
        let error = worker
            .exchange(
                ControlFrame::Prepare(bad),
                None,
                Instant::now() + Duration::from_secs(5),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            SupervisorError::Protocol(FrameError::CorrelationMismatch)
        ));
    }

    #[test]
    fn true_binary_proves_no_ready_without_a_model() {
        // /bin/true exits 0 but speaks no protocol: Prepare must still fail
        // on the missing model before any spawn.
        let mut worker = worker_with("/bin/true");
        let error = worker
            .exchange(
                ControlFrame::Prepare(correlation("hash")),
                None,
                Instant::now() + Duration::from_secs(5),
            )
            .unwrap_err();
        assert!(matches!(error, SupervisorError::LoadAborted));
    }

    #[test]
    fn native_failure_mapping_is_fail_closed() {
        let worker = worker_with("/bin/false");
        // Non-zero exit with no output: a native failure, never a transcript.
        assert!(matches!(
            worker.interpret_exit(Some(1), b"", b""),
            Err(SupervisorError::Crashed)
        ));
        // Exit 0 with an unparseable envelope is still a broken contract,
        // surfaced when the caller parses the returned bytes.
        let stdout = worker.interpret_exit(Some(0), b"not json", b"").unwrap();
        assert!(parse_whisper_json(&stdout).is_err());
        // Missing-model diagnostics fail closed as unavailable.
        assert!(matches!(
            worker.interpret_exit(Some(1), b"", b"failed to load model: nope"),
            Err(SupervisorError::LoadAborted)
        ));
        assert!(matches!(
            worker.interpret_exit(Some(1), b"", b"cannot open file"),
            Err(SupervisorError::LoadAborted)
        ));
    }

    #[test]
    fn oom_signatures_are_out_of_memory_never_a_transcript() {
        let worker = worker_with("/bin/false");
        assert!(matches!(
            worker.interpret_exit(None, b"", b"std::bad_alloc"),
            Err(SupervisorError::OutOfMemory)
        ));
        assert!(matches!(
            worker.interpret_exit(Some(1), b"", b"ggml: out of memory"),
            Err(SupervisorError::OutOfMemory)
        ));
        assert!(matches!(
            worker.interpret_exit(Some(1), b"", b"cannot allocate memory"),
            Err(SupervisorError::OutOfMemory)
        ));
        // External SIGKILL (the host OOM killer's signature). Our own
        // deadline kills return TimedOut before exit interpretation, so 137
        // here is never our own cleanup.
        assert!(matches!(
            worker.interpret_exit(Some(137), b"", b"Killed"),
            Err(SupervisorError::OutOfMemory)
        ));
        // A plain signal death without an OOM signature stays a crash.
        assert!(matches!(
            worker.interpret_exit(None, b"", b""),
            Err(SupervisorError::Crashed)
        ));
    }

    #[test]
    fn artifact_binding_rejects_unsafe_weights_names() {
        assert!(WhisperCppWorker::from_artifact(Path::new("/a"), "../evil.bin", "h").is_err());
        assert!(WhisperCppWorker::from_artifact(Path::new("/a"), "", "h").is_err());
        let worker =
            WhisperCppWorker::from_artifact(Path::new("/a"), "ggml-base.en.bin", "h").unwrap();
        assert_eq!(worker.model, PathBuf::from("/a/ggml-base.en.bin"));
    }

    #[test]
    fn transcribe_before_prepare_is_unavailable_not_a_spawn() {
        // Even the no-spawn silence path refuses work until first-inference
        // health has proven the model: Ready is never metadata-only.
        let mut worker = worker_with("/nonexistent/whisper-cli");
        let error = worker
            .exchange(
                ControlFrame::Transcribe {
                    correlation: correlation("hash"),
                    pcm_bytes: 4,
                },
                Some(&[0, 0, 0, 0]),
                Instant::now() + Duration::from_secs(5),
            )
            .unwrap_err();
        assert!(matches!(error, SupervisorError::Unavailable(_)));
        assert!(!worker.prepared);
    }

    #[test]
    fn declared_pcm_mismatch_is_rejected_before_spawn() {
        let mut worker = worker_with("/nonexistent/whisper-cli");
        let error = worker
            .exchange(
                ControlFrame::Transcribe {
                    correlation: correlation("hash"),
                    pcm_bytes: 99,
                },
                Some(&[1, 0]),
                Instant::now() + Duration::from_secs(5),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            SupervisorError::Protocol(FrameError::DeclaredPcmMismatch { .. })
        ));
    }

    #[test]
    fn wav_header_encodes_s16le_mono_16k() {
        let wav = wav_bytes(&[1, 0, 2, 0]).unwrap();
        assert_eq!(wav.len(), 48);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(u16::from_le_bytes([wav[20], wav[21]]), 1);
        assert_eq!(u16::from_le_bytes([wav[22], wav[23]]), 1);
        assert_eq!(
            u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]),
            16_000
        );
        assert_eq!(u16::from_le_bytes([wav[34], wav[35]]), 16);
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(&wav[44..], &[1, 0, 2, 0]);
        assert!(wav_bytes(&[0]).is_none());
    }

    #[test]
    fn whisper_json_concatenates_segments_and_maps_blank_to_no_text() {
        let value = serde_json::json!({
            "transcription": [
                {"text": " And so my fellow Americans,"},
                {"text": " ask not what your country can do for you."},
            ]
        });
        let bytes = serde_json::to_vec(&value).unwrap();
        match parse_whisper_json(&bytes).unwrap() {
            WhisperText::Transcript(text) => assert!(text.contains("fellow Americans")),
            WhisperText::NoText => panic!("expected transcript"),
        }
        let empty = serde_json::json!({"transcription": []});
        assert_eq!(
            parse_whisper_json(&serde_json::to_vec(&empty).unwrap()).unwrap(),
            WhisperText::NoText
        );
        let blank = serde_json::json!({"transcription": [{"text": "   "}]});
        assert_eq!(
            parse_whisper_json(&serde_json::to_vec(&blank).unwrap()).unwrap(),
            WhisperText::NoText
        );
    }

    #[test]
    fn whisper_json_rejects_broken_envelopes() {
        assert!(parse_whisper_json(b"not json").is_err());
        let missing = serde_json::json!({"result": []});
        assert!(parse_whisper_json(&serde_json::to_vec(&missing).unwrap()).is_err());
        let nul = serde_json::json!({"transcription": [{"text": "bad\0byte"}]});
        assert!(parse_whisper_json(&serde_json::to_vec(&nul).unwrap()).is_err());
    }

    #[test]
    fn cancel_is_clean_without_a_persistent_child() {
        let mut worker = worker_with("/bin/true");
        assert_eq!(worker.cancel_and_reap().unwrap(), ReapOutcome::Exited);
    }

    #[test]
    fn memfd_delivery_is_unnamed_and_roundtrips_bytes() {
        let wav = wav_bytes(&[1, 0, 2, 0]).expect("wav");
        let (held, path) = sealed_memfd_wav(&wav).expect("memfd");
        assert!(path.starts_with("/proc/self/fd/"), "unnamed: {path}");
        assert!(!path.contains("tmp"), "no temp file: {path}");
        let back = std::fs::read(&path).expect("child-visible bytes");
        assert_eq!(back, wav);
        drop(held);
    }

    #[test]
    fn read_capped_unblocks_a_blocked_writer_and_caps_noise() {
        use std::os::unix::net::UnixStream;
        let (mut writer, reader) = UnixStream::pair().expect("socket pair");
        let flood: Vec<u8> = (0..=255u8).cycle().take(2 * 1024 * 1024).collect();
        let producer = std::thread::spawn(move || {
            let mut written = 0;
            while written < flood.len() {
                match writer.write(&flood[written..]) {
                    Ok(0) => break,
                    Ok(n) => written += n,
                    Err(_) => break,
                }
            }
        });
        let mut reader = reader;
        let started = Instant::now();
        let (kept, truncated) = read_capped(&mut reader, 64 * 1024);
        // 2 MiB through a 64 KiB cap must return promptly with the writer
        // drained, never wedged on a full pipe. The kept length is
        // scheduling-dependent: the read that would cross the cap is
        // dropped whole, so a short pipe read can leave `kept` below the
        // cap. Assert the contract (cap respected, truncation reported),
        // never an exact size.
        assert!(started.elapsed() < Duration::from_secs(20));
        assert!(
            truncated,
            "2 MiB exceeds the cap; read_capped must report truncation"
        );
        assert!(
            kept.len() <= 64 * 1024,
            "kept {} bytes exceeds the 64 KiB cap",
            kept.len()
        );
        producer.join().expect("writer drained, not wedged");
    }

    /// Script stand-in for whisper-cli: verifies the `--file` argument opens
    /// and starts with a RIFF header (proving the memfd path is readable),
    /// counts invocations in a sidecar file, and emits a fixed JSON envelope.
    fn fake_whisper_dir() -> Option<tempfile::TempDir> {
        let python = ["/usr/bin/python3", "/usr/local/bin/python3"]
            .into_iter()
            .map(PathBuf::from)
            .find(|path| path.is_file())?;
        let dir = tempfile::tempdir().expect("test scratch");
        let script = dir.path().join("fake-whisper");
        std::fs::write(
            &script,
            format!(
                "#!{}\nimport json, sys\nargv = sys.argv[1:]\npath = argv[argv.index('--file') + 1]\nwith open(path, 'rb') as handle:\n    magic = handle.read(4)\nif magic != b'RIFF':\n    sys.stderr.write('not a wav\\n')\n    sys.exit(3)\nwith open(sys.argv[0] + '.count', 'a') as log:\n    log.write('run\\n')\nsys.stdout.write(json.dumps({{'transcription': [{{'text': 'hello memfd'}}]}}))\n",
                python.display()
            ),
        )
        .expect("script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
                .expect("chmod");
        }
        std::fs::write(dir.path().join("model.bin"), b"fixture-weights").expect("model");
        Some(dir)
    }

    fn fake_invocations(dir: &tempfile::TempDir) -> usize {
        let count = dir.path().join("fake-whisper.count");
        std::fs::read_to_string(&count)
            .unwrap_or_default()
            .lines()
            .count()
    }

    #[test]
    fn prepare_health_proves_inference_and_reports_cpu() {
        let Some(dir) = fake_whisper_dir() else {
            return;
        };
        let mut worker = WhisperCppWorker::new(
            dir.path().join("fake-whisper"),
            dir.path().join("model.bin"),
            1,
            "hash".into(),
        )
        .unwrap();
        let frame = worker
            .exchange(
                ControlFrame::Prepare(correlation("hash")),
                None,
                Instant::now() + Duration::from_secs(30),
            )
            .unwrap();
        match frame {
            WorkerFrame::Ready {
                observed_device, ..
            } => assert_eq!(observed_device, "cpu"),
            other => panic!("expected Ready, got {other:?}"),
        }
        assert!(worker.prepared);
        assert_eq!(worker.health_loads, 1);
        assert_eq!(fake_invocations(&dir), 1);
    }

    #[test]
    fn repeated_inference_reuses_health_without_reload_or_residue() {
        let Some(dir) = fake_whisper_dir() else {
            return;
        };
        let mut worker = WhisperCppWorker::new(
            dir.path().join("fake-whisper"),
            dir.path().join("model.bin"),
            1,
            "hash".into(),
        )
        .unwrap();
        worker
            .exchange(
                ControlFrame::Prepare(correlation("hash")),
                None,
                Instant::now() + Duration::from_secs(30),
            )
            .unwrap();
        let speech = vec![1u8, 0]
            .into_iter()
            .cycle()
            .take(200)
            .collect::<Vec<_>>();
        for request in ["q1", "q2"] {
            let mut id = correlation("hash");
            id.request_id = request.into();
            let frame = worker
                .exchange(
                    ControlFrame::Transcribe {
                        correlation: id,
                        pcm_bytes: speech.len(),
                    },
                    Some(&speech),
                    Instant::now() + Duration::from_secs(30),
                )
                .unwrap();
            assert!(
                matches!(frame, WorkerFrame::Transcript { ref text, .. } if text == "hello memfd")
            );
        }
        assert_eq!(worker.health_loads, 1);
        assert_eq!(fake_invocations(&dir), 3);
        // Silence after speech never spawns and never echoes old audio.
        let mut silent = correlation("hash");
        silent.request_id = "q3".into();
        let frame = worker
            .exchange(
                ControlFrame::Transcribe {
                    correlation: silent,
                    pcm_bytes: 4,
                },
                Some(&[0, 0, 0, 0]),
                Instant::now() + Duration::from_secs(5),
            )
            .unwrap();
        assert!(matches!(frame, WorkerFrame::NoText { .. }));
        assert_eq!(worker.health_loads, 1);
        assert_eq!(fake_invocations(&dir), 3);
    }

    #[test]
    fn broken_health_contract_never_reports_ready() {
        // /bin/true exits 0 but emits no envelope over the health run: with
        // a real model file present, Prepare still cannot claim Ready.
        let dir = tempfile::tempdir().expect("test scratch");
        std::fs::write(dir.path().join("model.bin"), b"fixture-weights").expect("model");
        let mut worker = WhisperCppWorker::new(
            PathBuf::from("/bin/true"),
            dir.path().join("model.bin"),
            1,
            "hash".into(),
        )
        .unwrap();
        let error = worker
            .exchange(
                ControlFrame::Prepare(correlation("hash")),
                None,
                Instant::now() + Duration::from_secs(10),
            )
            .unwrap_err();
        assert!(matches!(error, SupervisorError::Crashed));
        assert!(!worker.prepared);
    }
}
