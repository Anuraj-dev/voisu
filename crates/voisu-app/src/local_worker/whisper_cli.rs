//! Non-admitted Pilot Candidate worker using process-wrapped `whisper-cli`.
//!
//! One native executable, one verified model file, no Python/CUDA/FFI/JIT.
//! CPU-only, absolute paths, scrubbed environment, no downloader, no shell,
//! no caller-supplied paths. Each exchange spawns at most one child, drains
//! bounded stdout/stderr concurrently so a noisy child cannot wedge the pipe,
//! kills the child past the deadline, and reaps inline: nothing survives the
//! exchange, so `cancel_and_reap` is trivially clean.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::bounds::{MAX_PCM_BYTES, MAX_RETAINED_STDERR_BYTES, PCM_CHANNELS, PCM_SAMPLE_RATE_HZ};
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
        &self,
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
        Ok(WorkerFrame::Ready {
            correlation: correlation.clone(),
            observed_device: "cpu".into(),
        })
    }

    fn transcribe_inner(
        &self,
        correlation: &Correlation,
        pcm: &[u8],
        deadline: Instant,
    ) -> Result<WorkerFrame, SupervisorError> {
        Self::check_deadline(deadline)?;
        self.check_correlation(correlation)?;
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
        let request = tempfile::NamedTempFile::new()
            .map_err(|_| SupervisorError::Unavailable("worker cache unavailable"))?;
        std::fs::write(request.path(), &wav)
            .map_err(|_| SupervisorError::Unavailable("worker cache unavailable"))?;
        let stdout = self.run_cli(request.path(), deadline)?;
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

    fn run_cli(&self, wav: &Path, deadline: Instant) -> Result<Vec<u8>, SupervisorError> {
        let mut command = Command::new(&self.binary);
        command
            .args([
                "--model",
                &self.model.to_string_lossy(),
                "--file",
                &wav.to_string_lossy(),
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
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // No kill_on_drop on this toolchain: the poll loop below kills past
        // the deadline and every return path joins or reaps the child, so no
        // child outlives the exchange except via a host kill -9 of ourselves.
        let mut child = command.spawn().map_err(|_| SupervisorError::LoadAborted)?;
        let stdout_handle = child.stdout.take().map(|mut pipe| {
            std::thread::spawn(move || read_capped(&mut pipe, MAX_CLI_STDOUT_BYTES))
        });
        let stderr_handle = child.stderr.take().map(|mut pipe| {
            std::thread::spawn(move || read_capped(&mut pipe, MAX_CLI_STDERR_BYTES))
        });
        loop {
            match child.try_wait().map_err(|_| SupervisorError::Crashed)? {
                Some(status) => {
                    let (stdout, _) = join_reader(stdout_handle);
                    let (stderr, _) = join_reader(stderr_handle);
                    return self.interpret_exit(status.code(), &stdout, &stderr);
                }
                None => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        let _ = stdout_handle.map(|handle| handle.join());
                        let _ = stderr_handle.map(|handle| handle.join());
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
        if code == Some(0) {
            return Ok(stdout.to_vec());
        }
        let diagnostic = String::from_utf8_lossy(&stderr[..stderr.len().min(1024)]);
        let lowered = diagnostic.to_ascii_lowercase();
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

fn join_reader(handle: Option<std::thread::JoinHandle<(Vec<u8>, bool)>>) -> (Vec<u8>, bool) {
    handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default()
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
        // /bin/false exits 1 with no output: a native failure, never a transcript.
        let mut failing = worker_with("/bin/false");
        let error = failing
            .exchange(
                ControlFrame::Transcribe {
                    correlation: correlation("hash"),
                    pcm_bytes: 2,
                },
                Some(&[1, 0]),
                Instant::now() + Duration::from_secs(10),
            )
            .unwrap_err();
        assert!(matches!(error, SupervisorError::Crashed));
        // /bin/true exits 0 with empty stdout: a broken runtime contract.
        let mut empty = worker_with("/bin/true");
        let error = empty
            .exchange(
                ControlFrame::Transcribe {
                    correlation: correlation("hash"),
                    pcm_bytes: 2,
                },
                Some(&[1, 0]),
                Instant::now() + Duration::from_secs(10),
            )
            .unwrap_err();
        assert!(matches!(error, SupervisorError::Crashed));
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
    fn silence_never_spawns() {
        let mut worker = worker_with("/nonexistent/whisper-cli");
        let frame = worker
            .exchange(
                ControlFrame::Transcribe {
                    correlation: correlation("hash"),
                    pcm_bytes: 4,
                },
                Some(&[0, 0, 0, 0]),
                Instant::now() + Duration::from_secs(5),
            )
            .unwrap();
        assert!(matches!(frame, WorkerFrame::NoText { .. }));
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
}
