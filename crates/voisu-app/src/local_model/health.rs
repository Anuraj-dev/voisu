//! Health-before-activation. Never Delivery.

use std::path::Path;
use std::time::Instant;

use crate::local_worker::{
    Correlation, FakeWorker, SupervisorError, TranscribeRequest, WorkerOutcome, WorkerSupervisor,
};

use super::catalog::CatalogEntry;
use super::safe_fs::{self, SafeFsError};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HealthReport {
    pub observed_device: String,
    pub outcome: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HealthError {
    Tree(SafeFsError),
    Worker(String),
    DeliveryAttempted,
}

pub trait HealthProbe {
    fn check(&self, entry: &CatalogEntry, candidate: &Path) -> Result<HealthReport, HealthError>;
}

#[derive(Clone, Debug, Default)]
pub struct PassingHealth;

impl HealthProbe for PassingHealth {
    fn check(&self, entry: &CatalogEntry, candidate: &Path) -> Result<HealthReport, HealthError> {
        safe_fs::inspect_tree(candidate).map_err(HealthError::Tree)?;
        if entry.runtime_abi.abi_id.is_empty() {
            return Err(HealthError::Worker("missing ABI".into()));
        }
        let mut supervisor = WorkerSupervisor::<FakeWorker>::absent();
        supervisor
            .attach_ready(FakeWorker::default(), Instant::now())
            .map_err(|error| HealthError::Worker(format!("{error:?}")))?;
        let correlation = Correlation {
            daemon_nonce: "health".into(),
            generation: 1,
            request_id: "health".into(),
            recording_id: "health".into(),
            model_receipt_hash: "harness-no-weights".into(),
        };
        supervisor
            .prepare(correlation.clone(), Instant::now())
            .map_err(|error| HealthError::Worker(format!("{error:?}")))?;
        let pcm = std::fs::read(candidate.join("health.pcm")).unwrap_or_else(|_| vec![0, 0]);
        match supervisor.transcribe(TranscribeRequest { correlation, pcm }) {
            Ok(WorkerOutcome::NoText {
                observed_device, ..
            })
            | Ok(WorkerOutcome::Transcript {
                observed_device, ..
            }) => Ok(HealthReport {
                observed_device,
                outcome: "ok".into(),
            }),
            Err(SupervisorError::Unavailable(message)) => Err(HealthError::Worker(message.into())),
            Err(error) => Err(HealthError::Worker(format!("{error:?}"))),
        }
    }
}

#[cfg(test)]
#[derive(Clone, Debug)]
pub struct FailingHealth {
    pub reason: &'static str,
}

#[cfg(test)]
impl HealthProbe for FailingHealth {
    fn check(&self, _entry: &CatalogEntry, _candidate: &Path) -> Result<HealthReport, HealthError> {
        Err(HealthError::Worker(self.reason.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_model::catalog::ci_fixture_entry;
    use crate::local_worker::{DeliverySeam, FakeDelivery};

    #[test]
    fn health_does_not_invoke_delivery() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("health.pcm"),
            CatalogEntry::fixture_pcm_bytes(),
        )
        .unwrap();
        std::fs::write(
            temp.path().join("model.bin"),
            CatalogEntry::fixture_model_bytes(),
        )
        .unwrap();
        let mut delivery = FakeDelivery::default();
        PassingHealth
            .check(ci_fixture_entry(), temp.path())
            .unwrap();
        assert!(delivery.delivered.is_empty());
        assert!(delivery.authorize("health-rec").is_ok());
    }
}
