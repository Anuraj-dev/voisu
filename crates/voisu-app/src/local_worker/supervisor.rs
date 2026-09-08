//! Bounded worker supervisor seam (R5). One worker, one request.

use std::time::{Duration, Instant};

use super::bounds::{LOAD_DEADLINE, MAX_RESTARTS, RESTART_WINDOW, STOP_PROCESSING};
use super::protocol::{
    ControlFrame, Correlation, FrameError, WorkerFrame, correlations_match, is_silence_pcm,
    parse_control, parse_worker, validate_pcm, validate_transcript,
};
use super::runtime::{RuntimeError, reject_forbidden_program};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerState {
    Absent,
    Verifying,
    Loading,
    Ready,
    Busy,
    Stopping,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SupervisorError {
    NotReady(WorkerState),
    RestartExhausted,
    TimedOut,
    Protocol(FrameError),
    Runtime(RuntimeError),
    Busy,
    Unavailable(&'static str),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscribeRequest {
    pub correlation: Correlation,
    pub pcm: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkerOutcome {
    Transcript {
        text: String,
        observed_device: String,
    },
    NoText {
        reason: String,
        observed_device: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReapOutcome {
    Exited,
    Killed,
    Unreaped,
}

#[derive(Clone, Debug, Default)]
pub struct RestartBudget {
    starts: Vec<Instant>,
}

impl RestartBudget {
    pub fn try_register(&mut self, now: Instant) -> Result<(), SupervisorError> {
        self.starts
            .retain(|started| now.saturating_duration_since(*started) < RESTART_WINDOW);
        if self.starts.len() >= MAX_RESTARTS {
            return Err(SupervisorError::RestartExhausted);
        }
        self.starts.push(now);
        Ok(())
    }
}

pub trait WorkerChild {
    fn exchange(
        &mut self,
        control: ControlFrame,
        pcm: Option<&[u8]>,
    ) -> Result<WorkerFrame, SupervisorError>;
    fn cancel_and_reap(&mut self) -> Result<ReapOutcome, SupervisorError>;
}

/// In-memory worker used by the L2 harness. Does not load production weights.
#[derive(Clone, Debug)]
pub struct FakeWorker {
    pub generation: u64,
    pub model_receipt_hash: String,
    pub observed_device: String,
    pub scripted_text: Option<String>,
    pub terminal_sent: bool,
}

impl Default for FakeWorker {
    fn default() -> Self {
        Self {
            generation: 1,
            model_receipt_hash: "harness-no-weights".into(),
            observed_device: "cpu".into(),
            scripted_text: None,
            terminal_sent: false,
        }
    }
}

impl WorkerChild for FakeWorker {
    fn exchange(
        &mut self,
        control: ControlFrame,
        pcm: Option<&[u8]>,
    ) -> Result<WorkerFrame, SupervisorError> {
        match control {
            ControlFrame::Prepare(correlation) => {
                let expected = self.live_correlation(&correlation);
                correlations_match(&expected, &correlation).map_err(SupervisorError::Protocol)?;
                self.terminal_sent = false;
                Ok(WorkerFrame::Ready {
                    correlation,
                    observed_device: self.observed_device.clone(),
                })
            }
            ControlFrame::Transcribe {
                correlation,
                pcm_bytes,
            } => {
                if self.terminal_sent {
                    return Err(SupervisorError::Protocol(FrameError::DuplicateTerminal));
                }
                let pcm = pcm.unwrap_or_default();
                validate_pcm(pcm, Some(pcm_bytes)).map_err(SupervisorError::Protocol)?;
                if pcm_bytes != pcm.len() {
                    return Err(SupervisorError::Protocol(FrameError::ExtraAudio));
                }
                let expected = Correlation {
                    daemon_nonce: correlation.daemon_nonce.clone(),
                    generation: self.generation,
                    request_id: correlation.request_id.clone(),
                    recording_id: correlation.recording_id.clone(),
                    model_receipt_hash: self.model_receipt_hash.clone(),
                };
                correlations_match(&expected, &correlation).map_err(SupervisorError::Protocol)?;
                // One terminal frame per request; a warm worker accepts the next
                // Transcribe after this one completes.
                self.terminal_sent = true;
                if is_silence_pcm(pcm) {
                    self.terminal_sent = false;
                    return Ok(WorkerFrame::NoText {
                        correlation,
                        reason: "silence".into(),
                    });
                }
                if let Some(text) = &self.scripted_text {
                    validate_transcript(text).map_err(SupervisorError::Protocol)?;
                    if text.trim().is_empty() {
                        self.terminal_sent = false;
                        return Ok(WorkerFrame::NoText {
                            correlation,
                            reason: "empty".into(),
                        });
                    }
                    self.terminal_sent = false;
                    return Ok(WorkerFrame::Transcript {
                        correlation,
                        text: text.clone(),
                    });
                }
                self.terminal_sent = false;
                Ok(WorkerFrame::NoText {
                    correlation,
                    reason: "no-scripted-hypothesis".into(),
                })
            }
            ControlFrame::Cancel { .. } => Ok(WorkerFrame::Error {
                code: "cancelled".into(),
                metadata: serde_json::Map::new(),
            }),
        }
    }

    fn cancel_and_reap(&mut self) -> Result<ReapOutcome, SupervisorError> {
        Ok(ReapOutcome::Exited)
    }
}

impl FakeWorker {
    fn live_correlation(&self, offered: &Correlation) -> Correlation {
        Correlation {
            daemon_nonce: offered.daemon_nonce.clone(),
            generation: self.generation,
            request_id: offered.request_id.clone(),
            recording_id: offered.recording_id.clone(),
            model_receipt_hash: self.model_receipt_hash.clone(),
        }
    }
}

pub struct WorkerSupervisor<C> {
    state: WorkerState,
    child: Option<C>,
    restarts: RestartBudget,
    observed_device: String,
}

impl<C: WorkerChild> WorkerSupervisor<C> {
    #[must_use]
    pub fn absent() -> Self {
        Self {
            state: WorkerState::Absent,
            child: None,
            restarts: RestartBudget::default(),
            observed_device: "unknown".into(),
        }
    }

    #[must_use]
    pub fn state(&self) -> WorkerState {
        self.state
    }

    pub fn attach_ready(&mut self, child: C, now: Instant) -> Result<(), SupervisorError> {
        self.restarts.try_register(now)?;
        self.child = Some(child);
        self.state = WorkerState::Ready;
        Ok(())
    }

    pub fn prepare(
        &mut self,
        correlation: Correlation,
        now: Instant,
    ) -> Result<Duration, SupervisorError> {
        if matches!(self.state, WorkerState::Busy | WorkerState::Stopping) {
            return Err(SupervisorError::Busy);
        }
        let child = self
            .child
            .as_mut()
            .ok_or(SupervisorError::NotReady(self.state))?;
        self.state = WorkerState::Loading;
        let started = now;
        let frame = child.exchange(ControlFrame::Prepare(correlation), None)?;
        match frame {
            WorkerFrame::Ready {
                observed_device, ..
            } => {
                if started.elapsed() > LOAD_DEADLINE {
                    self.state = WorkerState::Unavailable;
                    return Err(SupervisorError::TimedOut);
                }
                self.observed_device = observed_device;
                self.state = WorkerState::Ready;
                Ok(started.elapsed())
            }
            _ => {
                self.state = WorkerState::Unavailable;
                Err(SupervisorError::Unavailable(
                    "prepare returned a non-ready frame",
                ))
            }
        }
    }

    pub fn transcribe(
        &mut self,
        request: TranscribeRequest,
    ) -> Result<WorkerOutcome, SupervisorError> {
        if self.state != WorkerState::Ready {
            return Err(SupervisorError::NotReady(self.state));
        }
        validate_pcm(&request.pcm, Some(request.pcm.len())).map_err(SupervisorError::Protocol)?;
        let child = self
            .child
            .as_mut()
            .ok_or(SupervisorError::NotReady(self.state))?;
        self.state = WorkerState::Busy;
        let started = Instant::now();
        let frame = child.exchange(
            ControlFrame::Transcribe {
                correlation: request.correlation,
                pcm_bytes: request.pcm.len(),
            },
            Some(&request.pcm),
        );
        if started.elapsed() > STOP_PROCESSING {
            self.state = WorkerState::Unavailable;
            return Err(SupervisorError::TimedOut);
        }
        let frame = frame?;
        self.state = WorkerState::Ready;
        match frame {
            WorkerFrame::Transcript { text, .. } => {
                if text.trim().is_empty() {
                    Ok(WorkerOutcome::NoText {
                        reason: "empty".into(),
                        observed_device: self.observed_device.clone(),
                    })
                } else {
                    Ok(WorkerOutcome::Transcript {
                        text,
                        observed_device: self.observed_device.clone(),
                    })
                }
            }
            WorkerFrame::NoText { reason, .. } => Ok(WorkerOutcome::NoText {
                reason,
                observed_device: self.observed_device.clone(),
            }),
            _ => {
                self.state = WorkerState::Unavailable;
                Err(SupervisorError::Unavailable(
                    "non-terminal transcribe frame",
                ))
            }
        }
    }

    pub fn cancel(&mut self) -> Result<ReapOutcome, SupervisorError> {
        self.state = WorkerState::Stopping;
        let outcome = match self.child.as_mut() {
            Some(child) => child.cancel_and_reap()?,
            None => ReapOutcome::Exited,
        };
        self.child = None;
        self.state = match outcome {
            ReapOutcome::Unreaped => WorkerState::Unavailable,
            ReapOutcome::Exited | ReapOutcome::Killed => WorkerState::Absent,
        };
        Ok(outcome)
    }
}

pub fn spawn_program_allowed(program: &std::path::Path) -> Result<(), SupervisorError> {
    reject_forbidden_program(program).map_err(SupervisorError::Runtime)
}

/// Parse a JSON value that has already passed frame bounds.
pub fn worker_frame_from_json(value: &serde_json::Value) -> Result<WorkerFrame, SupervisorError> {
    parse_worker(value).map_err(SupervisorError::Protocol)
}

pub fn control_frame_from_json(value: &serde_json::Value) -> Result<ControlFrame, SupervisorError> {
    parse_control(value).map_err(SupervisorError::Protocol)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corr() -> Correlation {
        Correlation {
            daemon_nonce: "n".into(),
            generation: 1,
            request_id: "q".into(),
            recording_id: "rec-1".into(),
            model_receipt_hash: "harness-no-weights".into(),
        }
    }

    fn ready_supervisor(text: Option<&str>) -> WorkerSupervisor<FakeWorker> {
        let worker = FakeWorker {
            scripted_text: text.map(ToOwned::to_owned),
            ..FakeWorker::default()
        };
        let mut supervisor = WorkerSupervisor::absent();
        supervisor.attach_ready(worker, Instant::now()).unwrap();
        supervisor.prepare(corr(), Instant::now()).unwrap();
        supervisor
    }

    #[test]
    fn silence_is_no_text_never_invented_success() {
        let mut supervisor = ready_supervisor(Some("should-not-be-used-for-silence"));
        let outcome = supervisor
            .transcribe(TranscribeRequest {
                correlation: corr(),
                pcm: vec![0, 0, 0, 0],
            })
            .unwrap();
        assert!(matches!(outcome, WorkerOutcome::NoText { .. }));
    }

    #[test]
    fn scripted_speech_returns_observed_cpu_not_requested_gpu() {
        let mut supervisor = ready_supervisor(Some("hello raja"));
        let pcm = vec![1, 0, 2, 0];
        let outcome = supervisor
            .transcribe(TranscribeRequest {
                correlation: corr(),
                pcm,
            })
            .unwrap();
        match outcome {
            WorkerOutcome::Transcript {
                text,
                observed_device,
            } => {
                assert_eq!(text, "hello raja");
                assert_eq!(observed_device, "cpu");
            }
            other => panic!("expected transcript, got {other:?}"),
        }
    }

    #[test]
    fn stale_generation_is_rejected() {
        let mut supervisor = ready_supervisor(Some("hi"));
        let mut correlation = corr();
        correlation.generation = 99;
        let error = supervisor
            .transcribe(TranscribeRequest {
                correlation,
                pcm: vec![1, 0],
            })
            .unwrap_err();
        assert!(matches!(
            error,
            SupervisorError::Protocol(FrameError::CorrelationMismatch)
        ));
    }

    #[test]
    fn fourth_start_in_five_minutes_stays_unavailable() {
        let mut budget = RestartBudget::default();
        let t0 = Instant::now();
        for _ in 0..MAX_RESTARTS {
            budget.try_register(t0).unwrap();
        }
        assert!(matches!(
            budget.try_register(t0 + Duration::from_secs(1)),
            Err(SupervisorError::RestartExhausted)
        ));
    }

    #[test]
    fn cancel_reaps_and_leaves_absent() {
        let mut supervisor = ready_supervisor(None);
        assert_eq!(supervisor.cancel().unwrap(), ReapOutcome::Exited);
        assert_eq!(supervisor.state(), WorkerState::Absent);
    }

    #[test]
    fn transcribe_before_ready_fails_without_capture_side_effects() {
        let supervisor = WorkerSupervisor::<FakeWorker>::absent();
        assert_eq!(supervisor.state(), WorkerState::Absent);
        let mut supervisor = supervisor;
        let error = supervisor
            .transcribe(TranscribeRequest {
                correlation: corr(),
                pcm: vec![1, 0],
            })
            .unwrap_err();
        assert!(matches!(
            error,
            SupervisorError::NotReady(WorkerState::Absent)
        ));
    }

    #[test]
    fn process_wrapped_worker_speaks_length_prefixed_protocol() {
        use std::io::{Read, Write};
        use std::process::{Command, Stdio};

        use crate::local_worker::protocol::{decode_json_frame, encode_json_frame};
        use crate::local_worker::sandbox::scrub_worker_environment;

        spawn_program_allowed(std::path::Path::new("/usr/bin/true")).unwrap();

        let script = r#"
import json, struct, sys

def read_exact(n):
    buf = bytearray()
    while len(buf) < n:
        chunk = sys.stdin.buffer.read(n - len(buf))
        if not chunk:
            return None
        buf.extend(chunk)
    return bytes(buf)

def read_frame():
    hdr = read_exact(4)
    if hdr is None:
        return None
    n = struct.unpack('<I', hdr)[0]
    if n == 0 or n > 65536:
        sys.exit(2)
    raw = read_exact(n)
    return json.loads(raw)

def write_frame(obj):
    raw = json.dumps(obj).encode()
    sys.stdout.buffer.write(struct.pack('<I', len(raw)))
    sys.stdout.buffer.write(raw)
    sys.stdout.buffer.flush()

prep = read_frame()
ids = {k: prep[k] for k in ('daemon_nonce', 'generation', 'request_id', 'recording_id', 'model_receipt_hash')}
ids.update({'v': 1, 'kind': 'ready', 'observed_device': 'cpu'})
write_frame(ids)
msg = read_frame()
pcm = read_exact(msg['pcm_bytes'])
out = {k: msg[k] for k in ('daemon_nonce', 'generation', 'request_id', 'recording_id', 'model_receipt_hash')}
out.update({'v': 1, 'kind': 'no_text', 'reason': 'silence'})
write_frame(out)
"#;
        let mut child = Command::new("python3")
            .args(["-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env_clear()
            .envs(scrub_worker_environment(std::env::vars_os()).retained)
            .spawn()
            .expect("python3 protocol stand-in");
        let mut stdin = child.stdin.take().expect("stdin");
        let mut stdout = child.stdout.take().expect("stdout");
        let prepare = serde_json::json!({
            "v": 1,
            "kind": "prepare",
            "daemon_nonce": "n",
            "generation": 1,
            "request_id": "q",
            "recording_id": "rec-1",
            "model_receipt_hash": "harness-no-weights",
        });
        stdin
            .write_all(&encode_json_frame(&prepare).unwrap())
            .unwrap();
        let transcribe = serde_json::json!({
            "v": 1,
            "kind": "transcribe",
            "daemon_nonce": "n",
            "generation": 1,
            "request_id": "q",
            "recording_id": "rec-1",
            "model_receipt_hash": "harness-no-weights",
            "pcm_bytes": 4,
        });
        stdin
            .write_all(&encode_json_frame(&transcribe).unwrap())
            .unwrap();
        stdin.write_all(&[0, 0, 0, 0]).unwrap();
        stdin.flush().unwrap();
        drop(stdin);

        let mut collected = Vec::new();
        stdout.read_to_end(&mut collected).unwrap();
        let status = child.wait().unwrap();
        assert!(
            status.success(),
            "process-wrapped stand-in failed: {status}"
        );
        let (ready, used) = decode_json_frame(&collected).unwrap();
        assert_eq!(ready["kind"], "ready");
        assert_eq!(ready["observed_device"], "cpu");
        let (terminal, _) = decode_json_frame(&collected[used..]).unwrap();
        let frame = worker_frame_from_json(&terminal).unwrap();
        assert!(matches!(frame, WorkerFrame::NoText { reason, .. } if reason == "silence"));
        assert!(control_frame_from_json(&prepare).is_ok());
    }
}
