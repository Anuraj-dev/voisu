//! Health-before-activation. Never Delivery.

use std::path::Path;
use std::time::Instant;

use crate::local_worker::{
    Correlation, FakeWorker, LauncherPolicy, RestrictionProbe, SupervisorError, TranscribeRequest,
    WorkerOutcome, WorkerSupervisor, landlock_allowlist, local_unavailable_if_restrictions_fail,
};

use super::catalog::{CatalogEntry, FileKind, sha256_hex, verify_file_digest};
use super::safe_fs::{self, SafeFsError};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HealthReport {
    pub observed_device: String,
    pub outcome: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HealthError {
    Tree(SafeFsError),
    MissingFile(&'static str),
    Digest(&'static str),
    UnexpectedFile,
    Abi,
    Sandbox,
    LoadAbort,
    FixtureQuality,
    Worker(String),
}

pub trait HealthProbe {
    fn check(&self, entry: &CatalogEntry, candidate: &Path) -> Result<HealthReport, HealthError>;
}

/// Production health: re-hash the candidate, apply launcher policy, then score
/// the packaged fixture through the same supervisor deadlines as inference.
#[derive(Clone, Debug)]
pub struct CandidateHealth {
    pub restrictions: RestrictionProbe,
}

impl Default for CandidateHealth {
    fn default() -> Self {
        Self {
            restrictions: RestrictionProbe::Available,
        }
    }
}

impl HealthProbe for CandidateHealth {
    fn check(&self, entry: &CatalogEntry, candidate: &Path) -> Result<HealthReport, HealthError> {
        verify_candidate(entry, candidate)?;
        if local_unavailable_if_restrictions_fail(self.restrictions) {
            return Err(HealthError::Sandbox);
        }
        let policy = LauncherPolicy::intended_production();
        if !policy.landlock_required || !policy.deny_ip_sockets || !policy.no_new_privs {
            return Err(HealthError::Sandbox);
        }
        let model_name = entry
            .files
            .iter()
            .find(|file| file.kind == FileKind::Weights)
            .map(|file| file.name)
            .ok_or(HealthError::MissingFile("model"))?;
        let allow = landlock_allowlist(candidate.join(model_name), candidate.join("cache"));
        if !allow.iter().any(|path| path.ends_with(model_name)) {
            return Err(HealthError::Sandbox);
        }
        let pcm_file = entry
            .files
            .iter()
            .find(|file| file.kind == FileKind::HealthFixture)
            .ok_or(HealthError::MissingFile("health.pcm"))?;
        let pcm = safe_fs::read_existing_file(candidate, pcm_file.name, pcm_file.bytes).map_err(
            |error| match error {
                SafeFsError::NotFound | SafeFsError::UnsafeName => {
                    HealthError::MissingFile(pcm_file.name)
                }
                other => HealthError::Tree(other),
            },
        )?;
        if pcm.as_slice() != CatalogEntry::fixture_pcm_bytes()
            || !verify_file_digest(&pcm, pcm_file.sha256_hex, pcm_file.bytes)
        {
            return Err(HealthError::FixtureQuality);
        }
        let model_file = entry
            .file(model_name)
            .ok_or(HealthError::MissingFile(model_name))?;
        let model = safe_fs::read_existing_file(candidate, model_name, model_file.bytes).map_err(
            |error| match error {
                SafeFsError::NotFound | SafeFsError::UnsafeName => HealthError::LoadAbort,
                other => HealthError::Tree(other),
            },
        )?;
        if !verify_file_digest(&model, model_file.sha256_hex, model_file.bytes) {
            return Err(HealthError::LoadAbort);
        }

        let receipt_hash = sha256_hex(&model);
        let mut worker = FakeWorker {
            generation: 1,
            model_receipt_hash: receipt_hash.clone(),
            scripted_text: None,
            ..FakeWorker::default()
        };
        if model.as_slice() != CatalogEntry::fixture_model_bytes() {
            worker.scripted_error = Some("load_abort".into());
        }
        let mut supervisor = WorkerSupervisor::<FakeWorker>::absent();
        supervisor
            .attach_ready(worker, Instant::now())
            .map_err(|error| HealthError::Worker(format!("{error:?}")))?;
        let correlation = Correlation {
            daemon_nonce: "health".into(),
            generation: 1,
            request_id: "health".into(),
            recording_id: "health".into(),
            model_receipt_hash: receipt_hash,
        };
        supervisor
            .prepare(correlation.clone(), Instant::now())
            .map_err(|error| match error {
                SupervisorError::LoadAborted => HealthError::LoadAbort,
                SupervisorError::TimedOut => HealthError::Worker("load deadline".into()),
                other => HealthError::Worker(format!("{other:?}")),
            })?;
        match supervisor.transcribe(TranscribeRequest { correlation, pcm }) {
            Ok(WorkerOutcome::NoText {
                observed_device, ..
            }) => Ok(HealthReport {
                observed_device,
                outcome: "no_text".into(),
            }),
            Ok(WorkerOutcome::Transcript { .. }) => Err(HealthError::FixtureQuality),
            Err(SupervisorError::LoadAborted) => Err(HealthError::LoadAbort),
            Err(SupervisorError::TimedOut) => Err(HealthError::FixtureQuality),
            Err(error) => Err(HealthError::Worker(format!("{error:?}"))),
        }
    }
}

pub fn verify_candidate(entry: &CatalogEntry, candidate: &Path) -> Result<(), HealthError> {
    if !entry.abi_supported() {
        return Err(HealthError::Abi);
    }
    safe_fs::inspect_tree(candidate).map_err(HealthError::Tree)?;
    let mut names = safe_fs::list_regular_names(candidate).map_err(HealthError::Tree)?;
    names.sort();
    let mut required = entry
        .required_names
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<Vec<_>>();
    required.sort();
    if names != required {
        if required.iter().any(|name| !names.contains(name)) {
            let missing = entry
                .required_names
                .iter()
                .copied()
                .find(|name| !names.iter().any(|have| have == name))
                .unwrap_or("required");
            return Err(HealthError::MissingFile(missing));
        }
        return Err(HealthError::UnexpectedFile);
    }
    for file in entry.files {
        let bytes =
            safe_fs::read_existing_file(candidate, file.name, file.bytes).map_err(|error| {
                match error {
                    SafeFsError::UnsafeName | SafeFsError::NotFound | SafeFsError::Io(_) => {
                        HealthError::MissingFile(file.name)
                    }
                    other => HealthError::Tree(other),
                }
            })?;
        if !verify_file_digest(&bytes, file.sha256_hex, file.bytes) {
            return Err(HealthError::Digest(file.name));
        }
    }
    Ok(())
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

    fn write_fixture(dir: &Path) {
        std::fs::write(dir.join("health.pcm"), CatalogEntry::fixture_pcm_bytes()).unwrap();
        std::fs::write(dir.join("model.bin"), CatalogEntry::fixture_model_bytes()).unwrap();
    }

    #[test]
    fn health_scores_fixture_without_delivery_types() {
        let temp = tempfile::tempdir().unwrap();
        write_fixture(temp.path());
        let report = CandidateHealth::default()
            .check(ci_fixture_entry(), temp.path())
            .unwrap();
        assert_eq!(report.outcome, "no_text");
        assert_eq!(report.observed_device, "cpu");
        let production = include_str!("health.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production source");
        assert!(!production.contains("FakeDelivery"));
        assert!(!production.contains("DeliverySeam"));
    }

    #[test]
    fn missing_health_pcm_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("model.bin"),
            CatalogEntry::fixture_model_bytes(),
        )
        .unwrap();
        let error = CandidateHealth::default()
            .check(ci_fixture_entry(), temp.path())
            .unwrap_err();
        assert!(matches!(error, HealthError::MissingFile("health.pcm")));
    }

    #[test]
    fn extra_file_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        write_fixture(temp.path());
        std::fs::write(temp.path().join("bonus.bin"), b"nope").unwrap();
        let error = CandidateHealth::default()
            .check(ci_fixture_entry(), temp.path())
            .unwrap_err();
        assert_eq!(error, HealthError::UnexpectedFile);
    }

    #[test]
    fn unloadable_model_fails_before_activation() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("health.pcm"),
            CatalogEntry::fixture_pcm_bytes(),
        )
        .unwrap();
        std::fs::write(temp.path().join("model.bin"), b"voisu-l3-fixture-XXXXX").unwrap();
        let error = CandidateHealth::default()
            .check(ci_fixture_entry(), temp.path())
            .unwrap_err();
        assert!(matches!(
            error,
            HealthError::Digest("model.bin") | HealthError::LoadAbort
        ));
    }

    #[test]
    fn unsupported_restrictions_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        write_fixture(temp.path());
        let health = CandidateHealth {
            restrictions: RestrictionProbe::Unsupported,
        };
        assert_eq!(
            health.check(ci_fixture_entry(), temp.path()),
            Err(HealthError::Sandbox)
        );
    }
}
