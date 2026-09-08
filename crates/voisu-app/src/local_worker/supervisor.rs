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
    Crashed,
    OutOfMemory,
    LoadAborted,
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
    /// Must return `TimedOut` instead of blocking past `deadline`.
    fn exchange(
        &mut self,
        control: ControlFrame,
        pcm: Option<&[u8]>,
        deadline: Instant,
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
    /// Simulated work duration. Compared to `deadline` without sleeping.
    pub block_for: Option<Duration>,
    /// If set, the worker echoes this correlation instead of the inbound one.
    pub response_correlation: Option<Correlation>,
    /// Scripted reap result. Default is a clean exit.
    pub reap: ReapOutcome,
    /// When set, Prepare/Transcribe returns this worker error code.
    pub scripted_error: Option<String>,
    /// Exchange fails as a crash instead of a protocol frame.
    pub crash: bool,
    /// Cancel does not reap; the child may still emit a late frame.
    pub ignore_cancel: bool,
}

impl Default for FakeWorker {
    fn default() -> Self {
        Self {
            generation: 1,
            model_receipt_hash: "harness-no-weights".into(),
            observed_device: "cpu".into(),
            scripted_text: None,
            block_for: None,
            response_correlation: None,
            reap: ReapOutcome::Exited,
            scripted_error: None,
            crash: false,
            ignore_cancel: false,
        }
    }
}

impl WorkerChild for FakeWorker {
    fn exchange(
        &mut self,
        control: ControlFrame,
        pcm: Option<&[u8]>,
        deadline: Instant,
    ) -> Result<WorkerFrame, SupervisorError> {
        if would_exceed_deadline(deadline, self.block_for) {
            return Err(SupervisorError::TimedOut);
        }
        if self.crash {
            return Err(SupervisorError::Crashed);
        }
        if let Some(code) = &self.scripted_error {
            return Ok(WorkerFrame::Error {
                code: code.clone(),
                metadata: serde_json::Map::new(),
            });
        }
        match control {
            ControlFrame::Prepare(correlation) => {
                let expected = self.live_correlation(&correlation);
                correlations_match(&expected, &correlation).map_err(SupervisorError::Protocol)?;
                Ok(WorkerFrame::Ready {
                    correlation: self.outbound_correlation(correlation),
                    observed_device: self.observed_device.clone(),
                })
            }
            ControlFrame::Transcribe {
                correlation,
                pcm_bytes,
            } => {
                let pcm = pcm.unwrap_or_default();
                validate_pcm(pcm, None).map_err(SupervisorError::Protocol)?;
                if pcm.len() > pcm_bytes {
                    return Err(SupervisorError::Protocol(FrameError::ExtraAudio));
                }
                if pcm.len() != pcm_bytes {
                    return Err(SupervisorError::Protocol(FrameError::DeclaredPcmMismatch {
                        declared: pcm_bytes,
                        actual: pcm.len(),
                    }));
                }
                let expected = Correlation {
                    daemon_nonce: correlation.daemon_nonce.clone(),
                    generation: self.generation,
                    request_id: correlation.request_id.clone(),
                    recording_id: correlation.recording_id.clone(),
                    model_receipt_hash: self.model_receipt_hash.clone(),
                };
                correlations_match(&expected, &correlation).map_err(SupervisorError::Protocol)?;
                let correlation = self.outbound_correlation(correlation);
                let frame = if is_silence_pcm(pcm) {
                    WorkerFrame::NoText {
                        correlation,
                        reason: "silence".into(),
                    }
                } else if let Some(text) = &self.scripted_text {
                    validate_transcript(text).map_err(SupervisorError::Protocol)?;
                    if text.trim().is_empty() {
                        WorkerFrame::NoText {
                            correlation,
                            reason: "empty".into(),
                        }
                    } else {
                        WorkerFrame::Transcript {
                            correlation,
                            text: text.clone(),
                        }
                    }
                } else {
                    WorkerFrame::NoText {
                        correlation,
                        reason: "no-scripted-hypothesis".into(),
                    }
                };
                Ok(frame)
            }
            ControlFrame::Cancel { .. } => Ok(WorkerFrame::Error {
                code: "cancelled".into(),
                metadata: serde_json::Map::new(),
            }),
        }
    }

    fn cancel_and_reap(&mut self) -> Result<ReapOutcome, SupervisorError> {
        if self.ignore_cancel {
            return Ok(ReapOutcome::Unreaped);
        }
        Ok(self.reap)
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

    fn outbound_correlation(&self, inbound: Correlation) -> Correlation {
        self.response_correlation.clone().unwrap_or(inbound)
    }
}

fn would_exceed_deadline(deadline: Instant, block_for: Option<Duration>) -> bool {
    let start = Instant::now();
    if start >= deadline {
        return true;
    }
    block_for.is_some_and(|block| deadline.saturating_duration_since(start) < block)
}

pub struct WorkerSupervisor<C> {
    state: WorkerState,
    child: Option<C>,
    restarts: RestartBudget,
    observed_device: String,
    last_terminal_request: Option<String>,
}

impl<C: WorkerChild> WorkerSupervisor<C> {
    #[must_use]
    pub fn absent() -> Self {
        Self {
            state: WorkerState::Absent,
            child: None,
            restarts: RestartBudget::default(),
            observed_device: "unknown".into(),
            last_terminal_request: None,
        }
    }

    #[must_use]
    pub fn state(&self) -> WorkerState {
        self.state
    }

    pub fn attach_ready(&mut self, child: C, now: Instant) -> Result<(), SupervisorError> {
        if self.child.is_some() {
            return Err(SupervisorError::Unavailable("worker still attached"));
        }
        self.restarts.try_register(now)?;
        self.child = Some(child);
        self.state = WorkerState::Ready;
        self.last_terminal_request = None;
        Ok(())
    }

    pub fn child_mut(&mut self) -> Option<&mut C> {
        self.child.as_mut()
    }

    fn conclude_exchange(
        &mut self,
        result: Result<WorkerFrame, SupervisorError>,
        deadline: Instant,
        on_protocol: WorkerState,
    ) -> Result<WorkerFrame, SupervisorError> {
        let timed_out =
            matches!(result, Err(SupervisorError::TimedOut)) || Instant::now() > deadline;
        if timed_out {
            return Err(self.timeout_and_reap());
        }
        match result {
            Ok(frame) => Ok(frame),
            Err(err @ SupervisorError::Protocol(_)) => {
                // Prepare protocol errors stay non-ready. Transcribe may return
                // Ready only after a valid correlated Ready already landed.
                self.state = on_protocol;
                Err(err)
            }
            Err(err @ SupervisorError::Crashed)
            | Err(err @ SupervisorError::OutOfMemory)
            | Err(err @ SupervisorError::LoadAborted) => Err(self.fail_and_reap(err)),
            Err(err) => {
                self.state = WorkerState::Unavailable;
                self.last_terminal_request = None;
                Err(err)
            }
        }
    }

    /// Bound a hung exchange: reap, keep unreaped children, else detach.
    fn timeout_and_reap(&mut self) -> SupervisorError {
        let outcome = match self.child.as_mut() {
            Some(child) => child.cancel_and_reap().unwrap_or(ReapOutcome::Unreaped),
            None => ReapOutcome::Exited,
        };
        self.apply_reap(outcome);
        SupervisorError::TimedOut
    }

    fn apply_reap(&mut self, outcome: ReapOutcome) {
        self.last_terminal_request = None;
        match outcome {
            ReapOutcome::Unreaped => {
                self.state = WorkerState::Unavailable;
            }
            ReapOutcome::Exited | ReapOutcome::Killed => {
                self.child = None;
                self.state = WorkerState::Absent;
            }
        }
    }

    fn protocol_reject(&mut self, err: FrameError) -> SupervisorError {
        self.state = WorkerState::Ready;
        SupervisorError::Protocol(err)
    }

    fn fail_and_reap(&mut self, err: SupervisorError) -> SupervisorError {
        let outcome = match self.child.as_mut() {
            Some(child) => child.cancel_and_reap().unwrap_or(ReapOutcome::Unreaped),
            None => ReapOutcome::Exited,
        };
        self.apply_reap(outcome);
        if self.state != WorkerState::Unavailable {
            self.state = WorkerState::Unavailable;
        }
        err
    }

    fn map_worker_error(&mut self, code: &str, unknown: WorkerState) -> SupervisorError {
        match code {
            "oom" => self.fail_and_reap(SupervisorError::OutOfMemory),
            "crash" => self.fail_and_reap(SupervisorError::Crashed),
            "load_abort" => self.fail_and_reap(SupervisorError::LoadAborted),
            _ => {
                self.state = unknown;
                SupervisorError::Protocol(FrameError::Unsolicited)
            }
        }
    }

    pub fn prepare(
        &mut self,
        correlation: Correlation,
        now: Instant,
    ) -> Result<Duration, SupervisorError> {
        self.prepare_until(correlation, now, now + LOAD_DEADLINE)
    }

    fn prepare_until(
        &mut self,
        correlation: Correlation,
        now: Instant,
        deadline: Instant,
    ) -> Result<Duration, SupervisorError> {
        if matches!(self.state, WorkerState::Busy | WorkerState::Stopping) {
            return Err(SupervisorError::Busy);
        }
        if self.child.is_none() {
            return Err(SupervisorError::NotReady(self.state));
        }
        self.state = WorkerState::Loading;
        let expected = correlation.clone();
        let exchanged = self.child.as_mut().expect("child checked").exchange(
            ControlFrame::Prepare(correlation),
            None,
            deadline,
        );
        let frame = self.conclude_exchange(exchanged, deadline, WorkerState::Unavailable)?;
        match frame {
            WorkerFrame::Ready {
                correlation,
                observed_device,
            } => {
                if let Err(err) = correlations_match(&expected, &correlation) {
                    self.state = WorkerState::Unavailable;
                    return Err(SupervisorError::Protocol(err));
                }
                self.observed_device = observed_device;
                self.state = WorkerState::Ready;
                Ok(now.elapsed())
            }
            WorkerFrame::Error { code, .. } => {
                Err(self.map_worker_error(&code, WorkerState::Unavailable))
            }
            _ => {
                self.state = WorkerState::Unavailable;
                self.last_terminal_request = None;
                Err(SupervisorError::Protocol(FrameError::Unsolicited))
            }
        }
    }

    pub fn transcribe(
        &mut self,
        request: TranscribeRequest,
    ) -> Result<WorkerOutcome, SupervisorError> {
        self.transcribe_until(request, Instant::now() + STOP_PROCESSING)
    }

    fn transcribe_until(
        &mut self,
        request: TranscribeRequest,
        deadline: Instant,
    ) -> Result<WorkerOutcome, SupervisorError> {
        if self.state != WorkerState::Ready {
            return Err(SupervisorError::NotReady(self.state));
        }
        validate_pcm(&request.pcm, Some(request.pcm.len())).map_err(SupervisorError::Protocol)?;
        let request_id = request.correlation.request_id.clone();
        if self.last_terminal_request.as_ref() == Some(&request_id) {
            return Err(SupervisorError::Protocol(FrameError::DuplicateTerminal));
        }
        if self.child.is_none() {
            return Err(SupervisorError::NotReady(self.state));
        }
        self.state = WorkerState::Busy;
        let expected = request.correlation.clone();
        let exchanged = self.child.as_mut().expect("child checked").exchange(
            ControlFrame::Transcribe {
                correlation: request.correlation,
                pcm_bytes: request.pcm.len(),
            },
            Some(&request.pcm),
            deadline,
        );
        let frame = self.conclude_exchange(exchanged, deadline, WorkerState::Ready)?;
        match frame {
            WorkerFrame::Transcript { text, correlation } => {
                if let Err(err) = correlations_match(&expected, &correlation) {
                    return Err(self.protocol_reject(err));
                }
                self.last_terminal_request = Some(request_id);
                self.state = WorkerState::Ready;
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
            WorkerFrame::NoText {
                reason,
                correlation,
            } => {
                if let Err(err) = correlations_match(&expected, &correlation) {
                    return Err(self.protocol_reject(err));
                }
                self.last_terminal_request = Some(request_id);
                self.state = WorkerState::Ready;
                Ok(WorkerOutcome::NoText {
                    reason,
                    observed_device: self.observed_device.clone(),
                })
            }
            WorkerFrame::Error { code, .. } => {
                Err(self.map_worker_error(&code, WorkerState::Ready))
            }
            WorkerFrame::Ready { .. } => {
                self.state = WorkerState::Ready;
                Err(SupervisorError::Protocol(FrameError::Unsolicited))
            }
        }
    }

    pub fn cancel(&mut self) -> Result<ReapOutcome, SupervisorError> {
        self.state = WorkerState::Stopping;
        let outcome = match self.child.as_mut() {
            Some(child) => child.cancel_and_reap()?,
            None => ReapOutcome::Exited,
        };
        self.apply_reap(outcome);
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
        assert_eq!(supervisor.state(), WorkerState::Ready);
        let follow_up = supervisor
            .transcribe(TranscribeRequest {
                correlation: corr(),
                pcm: vec![1, 0],
            })
            .unwrap();
        assert!(matches!(
            follow_up,
            WorkerOutcome::Transcript { ref text, .. } if text == "hi"
        ));
        assert_eq!(supervisor.state(), WorkerState::Ready);
    }

    #[test]
    fn duplicate_request_id_is_rejected_without_leaving_busy() {
        let mut supervisor = ready_supervisor(Some("hi"));
        supervisor
            .transcribe(TranscribeRequest {
                correlation: corr(),
                pcm: vec![1, 0],
            })
            .unwrap();
        let error = supervisor
            .transcribe(TranscribeRequest {
                correlation: corr(),
                pcm: vec![1, 0],
            })
            .unwrap_err();
        assert!(matches!(
            error,
            SupervisorError::Protocol(FrameError::DuplicateTerminal)
        ));
        assert_eq!(supervisor.state(), WorkerState::Ready);
        let mut next = corr();
        next.request_id = "q2".into();
        let outcome = supervisor
            .transcribe(TranscribeRequest {
                correlation: next,
                pcm: vec![1, 0],
            })
            .unwrap();
        assert!(matches!(outcome, WorkerOutcome::Transcript { .. }));
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
    fn transcribe_rejects_mismatched_response_correlation() {
        let mut supervisor = ready_supervisor(Some("secret"));
        if let Some(child) = supervisor.child_mut() {
            let mut echoed = corr();
            echoed.request_id = "other".into();
            child.response_correlation = Some(echoed);
        }
        let error = supervisor
            .transcribe(TranscribeRequest {
                correlation: corr(),
                pcm: vec![1, 0],
            })
            .unwrap_err();
        assert!(matches!(
            error,
            SupervisorError::Protocol(FrameError::CorrelationMismatch)
        ));
        assert_eq!(supervisor.state(), WorkerState::Ready);
        assert!(supervisor.child_mut().is_some());
    }

    #[test]
    fn prepare_rejects_ready_with_wrong_correlation() {
        let mut wrong = corr();
        wrong.generation = 99;
        let worker = FakeWorker {
            response_correlation: Some(wrong),
            ..FakeWorker::default()
        };
        let mut supervisor = WorkerSupervisor::absent();
        supervisor.attach_ready(worker, Instant::now()).unwrap();
        let error = supervisor.prepare(corr(), Instant::now()).unwrap_err();
        assert!(matches!(
            error,
            SupervisorError::Protocol(FrameError::CorrelationMismatch)
        ));
        assert_eq!(supervisor.state(), WorkerState::Unavailable);
        assert!(supervisor.child_mut().is_some());
        let transcribe = supervisor
            .transcribe(TranscribeRequest {
                correlation: corr(),
                pcm: vec![1, 0],
            })
            .unwrap_err();
        assert!(matches!(
            transcribe,
            SupervisorError::NotReady(WorkerState::Unavailable)
        ));
        if let Some(child) = supervisor.child_mut() {
            child.response_correlation = None;
        }
        supervisor.prepare(corr(), Instant::now()).unwrap();
        assert_eq!(supervisor.state(), WorkerState::Ready);
    }

    #[test]
    fn prepare_protocol_error_does_not_leave_ready() {
        let worker = FakeWorker {
            generation: 99,
            ..FakeWorker::default()
        };
        let mut supervisor = WorkerSupervisor::absent();
        supervisor.attach_ready(worker, Instant::now()).unwrap();
        let error = supervisor.prepare(corr(), Instant::now()).unwrap_err();
        assert!(matches!(
            error,
            SupervisorError::Protocol(FrameError::CorrelationMismatch)
        ));
        assert_eq!(supervisor.state(), WorkerState::Unavailable);
        assert!(supervisor.child_mut().is_some());
        let transcribe = supervisor
            .transcribe(TranscribeRequest {
                correlation: corr(),
                pcm: vec![1, 0],
            })
            .unwrap_err();
        assert!(matches!(
            transcribe,
            SupervisorError::NotReady(WorkerState::Unavailable)
        ));
    }

    #[test]
    fn unreaped_cancel_keeps_child_and_rejects_attach() {
        let worker = FakeWorker {
            reap: ReapOutcome::Unreaped,
            ..FakeWorker::default()
        };
        let mut supervisor = WorkerSupervisor::absent();
        supervisor.attach_ready(worker, Instant::now()).unwrap();
        supervisor.prepare(corr(), Instant::now()).unwrap();
        assert_eq!(supervisor.cancel().unwrap(), ReapOutcome::Unreaped);
        assert_eq!(supervisor.state(), WorkerState::Unavailable);
        assert!(supervisor.child_mut().is_some());
        let attach = supervisor
            .attach_ready(FakeWorker::default(), Instant::now())
            .unwrap_err();
        assert!(matches!(attach, SupervisorError::Unavailable(_)));
        assert!(supervisor.child_mut().is_some());
        assert_eq!(supervisor.state(), WorkerState::Unavailable);
        if let Some(child) = supervisor.child_mut() {
            child.reap = ReapOutcome::Exited;
        }
        assert_eq!(supervisor.cancel().unwrap(), ReapOutcome::Exited);
        assert_eq!(supervisor.state(), WorkerState::Absent);
        assert!(supervisor.child_mut().is_none());
        supervisor
            .attach_ready(FakeWorker::default(), Instant::now())
            .unwrap();
        assert_eq!(supervisor.state(), WorkerState::Ready);
    }

    #[test]
    fn unreaped_timeout_keeps_child_and_rejects_attach() {
        let worker = FakeWorker {
            block_for: Some(Duration::from_millis(20)),
            reap: ReapOutcome::Unreaped,
            ..FakeWorker::default()
        };
        let mut supervisor = WorkerSupervisor::absent();
        supervisor.attach_ready(worker, Instant::now()).unwrap();
        let error = supervisor
            .prepare_until(
                corr(),
                Instant::now(),
                Instant::now() + Duration::from_millis(1),
            )
            .unwrap_err();
        assert!(matches!(error, SupervisorError::TimedOut));
        assert_eq!(supervisor.state(), WorkerState::Unavailable);
        assert!(supervisor.child_mut().is_some());
        let attach = supervisor
            .attach_ready(FakeWorker::default(), Instant::now())
            .unwrap_err();
        assert!(matches!(attach, SupervisorError::Unavailable(_)));
        assert!(supervisor.child_mut().is_some());
        if let Some(child) = supervisor.child_mut() {
            child.reap = ReapOutcome::Exited;
        }
        assert_eq!(supervisor.cancel().unwrap(), ReapOutcome::Exited);
        assert_eq!(supervisor.state(), WorkerState::Absent);
        assert!(supervisor.child_mut().is_none());
    }

    #[test]
    fn hung_exchange_times_out_and_reaps_without_staying_busy() {
        let worker = FakeWorker {
            block_for: Some(Duration::from_millis(20)),
            scripted_text: Some("hi".into()),
            ..FakeWorker::default()
        };
        let mut supervisor = WorkerSupervisor::absent();
        supervisor.attach_ready(worker, Instant::now()).unwrap();
        supervisor
            .prepare_until(
                corr(),
                Instant::now(),
                Instant::now() + Duration::from_millis(50),
            )
            .unwrap();
        let error = supervisor
            .transcribe_until(
                TranscribeRequest {
                    correlation: corr(),
                    pcm: vec![1, 0],
                },
                Instant::now() + Duration::from_millis(1),
            )
            .unwrap_err();
        assert!(matches!(error, SupervisorError::TimedOut));
        assert_eq!(supervisor.state(), WorkerState::Absent);
        assert!(supervisor.child_mut().is_none());
        let follow = supervisor
            .transcribe(TranscribeRequest {
                correlation: corr(),
                pcm: vec![1, 0],
            })
            .unwrap_err();
        assert!(matches!(
            follow,
            SupervisorError::NotReady(WorkerState::Absent)
        ));
    }

    #[test]
    fn hung_prepare_times_out_and_does_not_stay_loading() {
        let worker = FakeWorker {
            block_for: Some(Duration::from_millis(20)),
            ..FakeWorker::default()
        };
        let mut supervisor = WorkerSupervisor::absent();
        supervisor.attach_ready(worker, Instant::now()).unwrap();
        let error = supervisor
            .prepare_until(
                corr(),
                Instant::now(),
                Instant::now() + Duration::from_millis(1),
            )
            .unwrap_err();
        assert!(matches!(error, SupervisorError::TimedOut));
        assert_eq!(supervisor.state(), WorkerState::Absent);
        assert!(supervisor.child_mut().is_none());
    }

    #[test]
    fn process_wrapped_worker_speaks_length_prefixed_protocol() {
        use std::io::{Read, Write};
        use std::path::PathBuf;
        use std::process::{Command, Stdio};
        use std::thread;

        use crate::local_worker::protocol::{decode_json_frame, encode_json_frame};
        use crate::local_worker::runtime::whisper_cpp_paths;
        use crate::local_worker::sandbox::scrub_worker_environment;

        spawn_program_allowed(std::path::Path::new("/usr/bin/true")).unwrap();
        assert!(
            std::env::var_os("VOISU_L2_SPAWN_WHISPER").is_none(),
            "CI must not spawn whisper.cpp; VOISU_L2_SPAWN_WHISPER is a host-only gate"
        );
        assert!(
            whisper_cpp_paths().binary.is_none() || whisper_cpp_paths().model.is_none(),
            "CI must not inject a whisper.cpp model; production weights stay undownloaded"
        );

        let python = ["/usr/bin/python3", "/usr/local/bin/python3"]
            .into_iter()
            .map(PathBuf::from)
            .find(|path| path.is_file());
        let Some(python) = python else {
            return;
        };

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
        let mut child = Command::new(&python)
            .args(["-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear()
            .envs(scrub_worker_environment(std::env::vars_os()).retained)
            .spawn()
            .expect("python3 protocol stand-in");
        let mut stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let mut stderr = child.stderr.take().expect("stderr");
        let stderr_thread = thread::spawn(move || {
            let mut drained = Vec::new();
            let mut buf = [0_u8; 256];
            loop {
                match stderr.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if drained.len() < crate::local_worker::MAX_RETAINED_STDERR_BYTES {
                            let room =
                                crate::local_worker::MAX_RETAINED_STDERR_BYTES - drained.len();
                            drained.extend_from_slice(&buf[..n.min(room)]);
                        }
                    }
                    Err(_) => break,
                }
            }
            drained
        });
        let stdout_thread = thread::spawn(move || {
            let mut collected = Vec::new();
            let mut stdout = stdout;
            let _ = stdout.read_to_end(&mut collected);
            collected
        });
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

        let deadline = Instant::now() + STOP_PROCESSING;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let reap_deadline = Instant::now() + crate::local_worker::REAP_OBSERVE;
                    while Instant::now() < reap_deadline {
                        if let Ok(Some(status)) = child.try_wait() {
                            panic!("process-wrapped stand-in exceeded STOP_PROCESSING: {status}");
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    panic!("process-wrapped stand-in unreaped after REAP_OBSERVE");
                }
                Ok(None) => thread::sleep(Duration::from_millis(10)),
                Err(error) => panic!("wait failed: {error}"),
            }
        };
        assert!(
            status.success(),
            "process-wrapped stand-in failed: {status}"
        );
        let collected = stdout_thread.join().expect("stdout reader");
        let _ = stderr_thread.join();
        let (ready, used) = decode_json_frame(&collected).unwrap();
        assert_eq!(ready["kind"], "ready");
        assert_eq!(ready["observed_device"], "cpu");
        let (terminal, _) = decode_json_frame(&collected[used..]).unwrap();
        let frame = worker_frame_from_json(&terminal).unwrap();
        assert!(matches!(frame, WorkerFrame::NoText { reason, .. } if reason == "silence"));
        assert!(control_frame_from_json(&prepare).is_ok());
    }
}
