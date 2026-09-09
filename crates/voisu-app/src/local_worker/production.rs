//! Production Local worker placeholder. Always unavailable while model
//! selection is evidence-gated. No scripted text, environment seam, or weights.

use std::time::Instant;

use super::protocol::{ControlFrame, WorkerFrame};
use super::supervisor::{ReapOutcome, SupervisorError, WorkerChild};

/// Production worker stand-in. The Gate never attaches it; it exists only so
/// production types do not monomorphize `FakeWorker`.
#[derive(Clone, Debug, Default)]
pub struct UnavailableWorker;

impl WorkerChild for UnavailableWorker {
    fn exchange(
        &mut self,
        _control: ControlFrame,
        _pcm: Option<&[u8]>,
        _deadline: Instant,
    ) -> Result<WorkerFrame, SupervisorError> {
        Err(SupervisorError::Unavailable(
            "local worker unavailable until a production model is selected",
        ))
    }

    fn cancel_and_reap(&mut self) -> Result<ReapOutcome, SupervisorError> {
        Ok(ReapOutcome::Exited)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_worker_never_yields_a_transcript() {
        let mut worker = UnavailableWorker;
        let correlation = super::super::protocol::Correlation {
            daemon_nonce: "l4".into(),
            generation: 1,
            request_id: "q".into(),
            recording_id: "rec-1".into(),
            model_receipt_hash: "h".into(),
        };
        let result = worker.exchange(
            ControlFrame::Prepare(correlation),
            None,
            Instant::now() + std::time::Duration::from_secs(1),
        );
        assert!(matches!(result, Err(SupervisorError::Unavailable(_))));
    }
}
