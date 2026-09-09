//! Production capture, supervision, and Delivery interfaces as fakes.
//!
//! The feasibility runner talks only to these seams so L6 can later substitute
//! the shipped capture/supervisor/Delivery without rewriting scoring.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use voisu_core::Transcript;

use super::protocol::Correlation;
use super::sandbox::CloudCapabilitySentinel;
use super::supervisor::{
    SupervisorError, TranscribeRequest, WorkerChild, WorkerOutcome, WorkerState, WorkerSupervisor,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FinalizedRecording {
    pub recording_id: String,
    pub pcm: Vec<u8>,
    pub recording_start: Instant,
    pub utterance_end: Instant,
}

pub trait CaptureSeam {
    fn finalize(
        &mut self,
        recording_id: &str,
        pcm: &[u8],
        speech: Duration,
    ) -> Result<FinalizedRecording, SeamError>;
}

pub trait DeliverySeam {
    fn authorize(&mut self, recording_id: &str) -> Result<DeliveryToken, SeamError>;
    fn deliver(&mut self, token: DeliveryToken, transcript: &Transcript) -> Result<(), SeamError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SeamError {
    Capture(&'static str),
    DuplicateDelivery,
    StaleToken,
    Unauthorized,
    CloudCapabilityUsed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeliveryToken {
    recording_id: String,
    generation: u64,
}

#[derive(Clone, Debug, Default)]
pub struct FakeCapture;

impl CaptureSeam for FakeCapture {
    fn finalize(
        &mut self,
        recording_id: &str,
        pcm: &[u8],
        speech: Duration,
    ) -> Result<FinalizedRecording, SeamError> {
        let utterance_end = Instant::now();
        let recording_start = utterance_end.checked_sub(speech).unwrap_or(utterance_end);
        Ok(FinalizedRecording {
            recording_id: recording_id.to_owned(),
            pcm: pcm.to_vec(),
            recording_start,
            utterance_end,
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct FakeDelivery {
    pub delivered: Vec<String>,
    spent: BTreeSet<String>,
    next_generation: u64,
}

impl DeliverySeam for FakeDelivery {
    fn authorize(&mut self, recording_id: &str) -> Result<DeliveryToken, SeamError> {
        if self.spent.contains(recording_id) {
            return Err(SeamError::DuplicateDelivery);
        }
        self.next_generation += 1;
        Ok(DeliveryToken {
            recording_id: recording_id.to_owned(),
            generation: self.next_generation,
        })
    }

    fn deliver(&mut self, token: DeliveryToken, transcript: &Transcript) -> Result<(), SeamError> {
        if self.spent.contains(&token.recording_id) {
            return Err(SeamError::DuplicateDelivery);
        }
        if token.generation != self.next_generation {
            return Err(SeamError::StaleToken);
        }
        self.spent.insert(token.recording_id);
        self.delivered.push(transcript.0.clone());
        Ok(())
    }
}

pub fn transcribe_through_supervisor<C: WorkerChild>(
    supervisor: &mut WorkerSupervisor<C>,
    correlation: Correlation,
    pcm: Vec<u8>,
    sentinel: &CloudCapabilitySentinel,
) -> Result<WorkerOutcome, SupervisorError> {
    if !sentinel.local_path_clean() {
        return Err(SupervisorError::Unavailable(
            "Cloud capability used on the Local path",
        ));
    }
    if supervisor.state() != WorkerState::Ready {
        return Err(SupervisorError::NotReady(supervisor.state()));
    }
    supervisor.transcribe(TranscribeRequest { correlation, pcm })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_worker::protocol::Correlation;
    use crate::local_worker::supervisor::FakeWorker;
    use std::time::Instant;

    fn corr() -> Correlation {
        Correlation {
            daemon_nonce: "n".into(),
            generation: 1,
            request_id: "q".into(),
            recording_id: "rec-1".into(),
            model_receipt_hash: "harness-no-weights".into(),
        }
    }

    #[test]
    fn delivery_is_single_use() {
        let mut delivery = FakeDelivery::default();
        let token = delivery.authorize("rec-1").unwrap();
        delivery
            .deliver(token.clone(), &Transcript("hello".into()))
            .unwrap();
        assert_eq!(
            delivery.deliver(token, &Transcript("hello".into())),
            Err(SeamError::DuplicateDelivery)
        );
        assert_eq!(delivery.delivered, vec!["hello".to_owned()]);
        assert_eq!(
            delivery.authorize("rec-1"),
            Err(SeamError::DuplicateDelivery)
        );
        let rec2 = delivery.authorize("rec-2").unwrap();
        delivery
            .deliver(rec2, &Transcript("second".into()))
            .unwrap();
        assert_eq!(
            delivery.delivered,
            vec!["hello".to_owned(), "second".to_owned()]
        );
    }

    #[test]
    fn stale_delivery_token_is_rejected() {
        let mut delivery = FakeDelivery::default();
        let stale = delivery.authorize("rec-1").unwrap();
        let _fresh = delivery.authorize("rec-2").unwrap();
        assert_eq!(
            delivery.deliver(stale, &Transcript("late".into())),
            Err(SeamError::StaleToken)
        );
        assert!(delivery.delivered.is_empty());
    }

    #[test]
    fn capture_stop_timestamp_is_the_utterance_end() {
        let mut capture = FakeCapture;
        let finalized = capture
            .finalize("rec-1", &[0, 0], Duration::from_millis(2_500))
            .unwrap();
        let speech = finalized
            .utterance_end
            .saturating_duration_since(finalized.recording_start);
        assert!(speech >= Duration::from_millis(2_400));
        assert!(speech <= Duration::from_millis(2_600));
    }

    #[test]
    fn local_path_refuses_work_if_cloud_sentinel_fired() {
        let mut sentinel = CloudCapabilitySentinel::new();
        sentinel.construct_cloud_client();
        let mut supervisor = WorkerSupervisor::<FakeWorker>::absent();
        supervisor
            .attach_ready(FakeWorker::default(), Instant::now())
            .unwrap();
        supervisor.prepare(corr(), Instant::now()).unwrap();
        let error = transcribe_through_supervisor(&mut supervisor, corr(), vec![1, 0], &sentinel)
            .unwrap_err();
        assert!(matches!(error, SupervisorError::Unavailable(_)));
    }
}
