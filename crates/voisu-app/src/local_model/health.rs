//! Health-before-activation. Never Delivery.

use std::path::Path;
use std::time::Instant;

use crate::local_worker::{
    Correlation, FakeWorker, LauncherPolicy, RestrictionProbe, SupervisorError, TranscribeRequest,
    WhisperCppWorker, WorkerChild, WorkerOutcome, WorkerSupervisor, landlock_allowlist,
    local_unavailable_if_restrictions_fail,
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
        if entry.production_weights {
            return self.check_production_inference(entry, candidate, model_name);
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

impl CandidateHealth {
    /// Real first-inference health for production entries: load the candidate
    /// model in a real worker and transcribe the packaged known-audio
    /// fixture through the same supervisor deadlines as inference. Never
    /// Delivery. The JFK fixture must come back as the known sentence; any
    /// other outcome fails closed.
    fn check_production_inference(
        &self,
        entry: &CatalogEntry,
        candidate: &Path,
        model_name: &str,
    ) -> Result<HealthReport, HealthError> {
        let fixture = entry
            .files
            .iter()
            .find(|file| file.kind == FileKind::HealthFixture)
            .ok_or(HealthError::MissingFile("health fixture"))?;
        let wav = safe_fs::read_existing_file(candidate, fixture.name, fixture.bytes).map_err(
            |error| match error {
                SafeFsError::NotFound | SafeFsError::UnsafeName => {
                    HealthError::MissingFile(fixture.name)
                }
                other => HealthError::Tree(other),
            },
        )?;
        if !verify_file_digest(&wav, fixture.sha256_hex, fixture.bytes) {
            return Err(HealthError::Digest(fixture.name));
        }
        let pcm = jfk_pcm(&wav).ok_or(HealthError::FixtureQuality)?;
        let worker =
            WhisperCppWorker::from_artifact(candidate, model_name, &format!("health-{}", entry.id))
                .map_err(|_| HealthError::Sandbox)?;
        score_known_audio(
            worker,
            pcm,
            &format!("health-{}", entry.id),
            "fellow americans",
        )
    }
}

/// Drive one Prepare + Transcribe health exchange through any worker. The
/// known-audio fixture must produce its sentence; silence, wrong text, or a
/// worker failure fails closed. Generic over `WorkerChild` so tests score the
/// gate with a scripted worker and never need the native runtime.
fn score_known_audio<W: WorkerChild>(
    worker: W,
    pcm: Vec<u8>,
    receipt_hash: &str,
    expected_phrase: &str,
) -> Result<HealthReport, HealthError> {
    let correlation = Correlation {
        daemon_nonce: "health".into(),
        generation: 1,
        request_id: "health".into(),
        recording_id: "health".into(),
        model_receipt_hash: receipt_hash.to_owned(),
    };
    let mut supervisor = WorkerSupervisor::absent();
    supervisor
        .attach_ready(worker, Instant::now())
        .map_err(|error| HealthError::Worker(format!("{error:?}")))?;
    supervisor
        .prepare(correlation.clone(), Instant::now())
        .map_err(|error| match error {
            SupervisorError::LoadAborted => HealthError::LoadAbort,
            SupervisorError::TimedOut => HealthError::Worker("health deadline".into()),
            other => HealthError::Worker(format!("{other:?}")),
        })?;
    match supervisor.transcribe(TranscribeRequest { correlation, pcm }) {
        Ok(WorkerOutcome::Transcript {
            text,
            observed_device,
        }) => {
            if text.to_ascii_lowercase().contains(expected_phrase) {
                Ok(HealthReport {
                    observed_device,
                    outcome: "transcript".into(),
                })
            } else {
                Err(HealthError::FixtureQuality)
            }
        }
        Ok(WorkerOutcome::NoText { .. }) => Err(HealthError::FixtureQuality),
        Err(SupervisorError::LoadAborted) => Err(HealthError::LoadAbort),
        Err(SupervisorError::TimedOut) => Err(HealthError::Worker("health deadline".into())),
        Err(error) => Err(HealthError::Worker(format!("{error:?}"))),
    }
}

/// Extract s16le mono 16 kHz PCM from a RIFF/WAV fixture by walking its
/// chunks. Extra informational chunks (the pinned JFK file carries a LIST
/// chunk before `data`) are skipped; anything else fails closed as fixture
/// quality, never as inference input.
fn jfk_pcm(wav: &[u8]) -> Option<Vec<u8>> {
    if wav.len() < 20 || !wav.len().is_multiple_of(2) {
        return None;
    }
    if &wav[0..4] != b"RIFF" || &wav[8..12] != b"WAVE" {
        return None;
    }
    let mut pos = 12;
    let mut fmt_ok = false;
    let mut data: Option<&[u8]> = None;
    while pos + 8 <= wav.len() {
        let id = &wav[pos..pos + 4];
        let size = u32::from_le_bytes(wav[pos + 4..pos + 8].try_into().ok()?) as usize;
        let start = pos + 8;
        let end = start.checked_add(size)?;
        if end > wav.len() {
            return None;
        }
        if id == b"fmt " {
            if size < 16 {
                return None;
            }
            if u16::from_le_bytes([wav[start], wav[start + 1]]) != 1
                || u16::from_le_bytes([wav[start + 2], wav[start + 3]]) != 1
                || u32::from_le_bytes(wav[start + 4..start + 8].try_into().ok()?) != 16_000
                || u16::from_le_bytes([wav[start + 14], wav[start + 15]]) != 16
            {
                return None;
            }
            fmt_ok = true;
        } else if id == b"data" {
            if data.is_some() {
                return None;
            }
            data = Some(&wav[start..end]);
        }
        pos = end + (size % 2);
    }
    if !fmt_ok {
        return None;
    }
    let data = data?;
    if data.is_empty() || !data.len().is_multiple_of(2) || pos != wav.len() {
        return None;
    }
    Some(data.to_vec())
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

    fn known_audio_worker(text: Option<&str>) -> FakeWorker {
        FakeWorker {
            generation: 1,
            model_receipt_hash: "health-fixture".into(),
            scripted_text: text.map(ToOwned::to_owned),
            ..FakeWorker::default()
        }
    }

    #[test]
    fn known_audio_gate_passes_only_on_the_expected_sentence() {
        let pcm = vec![1, 0, 2, 0];
        let report = score_known_audio(
            known_audio_worker(Some("And so my fellow Americans, ask not")),
            pcm.clone(),
            "health-fixture",
            "fellow americans",
        )
        .unwrap();
        assert_eq!(report.outcome, "transcript");
        assert_eq!(report.observed_device, "cpu");
        let wrong = score_known_audio(
            known_audio_worker(Some("something entirely different")),
            pcm.clone(),
            "health-fixture",
            "fellow americans",
        )
        .unwrap_err();
        assert_eq!(wrong, HealthError::FixtureQuality);
        let silent = score_known_audio(
            known_audio_worker(None),
            pcm,
            "health-fixture",
            "fellow americans",
        )
        .unwrap_err();
        assert_eq!(silent, HealthError::FixtureQuality);
    }

    #[test]
    fn tampered_production_candidate_fails_before_any_inference() {
        let temp = tempfile::tempdir().unwrap();
        let catalog = crate::local_model::shipped_catalog();
        let candidate = crate::local_model::pilot_candidate(&catalog).expect("Pilot Candidate");
        std::fs::write(temp.path().join("ggml-base.en.bin"), b"tampered").unwrap();
        std::fs::write(temp.path().join("jfk.wav"), b"tampered").unwrap();
        let error = CandidateHealth::default()
            .check(candidate, temp.path())
            .unwrap_err();
        assert!(
            matches!(error, HealthError::Digest(_) | HealthError::MissingFile(_)),
            "{error:?}"
        );
    }

    #[test]
    fn wav_fixture_must_be_s16le_mono_16k() {
        assert!(jfk_pcm(b"too short").is_none());
        let mut bad = vec![0u8; 48];
        bad[0..4].copy_from_slice(b"RIFX");
        assert!(jfk_pcm(&bad).is_none());
    }

    fn chunked_fixture(extra: &[u8]) -> Vec<u8> {
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        let pcm = [7u8, 0, 8, 0];
        let total = 4 + (8 + 16) + extra.len() + (8 + pcm.len());
        wav.extend_from_slice(&(total as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&16_000u32.to_le_bytes());
        wav.extend_from_slice(&32_000u32.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(extra);
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
        wav.extend_from_slice(&pcm);
        wav
    }

    #[test]
    fn wav_walker_skips_list_chunks_and_rejects_bad_format() {
        let mut list = Vec::new();
        list.extend_from_slice(b"LIST");
        list.extend_from_slice(&26u32.to_le_bytes());
        list.extend_from_slice(&[0u8; 26]);
        let pcm = jfk_pcm(&chunked_fixture(&list)).expect("LIST is skipped");
        assert_eq!(pcm, vec![7, 0, 8, 0]);
        assert!(jfk_pcm(&chunked_fixture(&[])).is_some());
        let mut stereo = chunked_fixture(&[]);
        stereo[22] = 2;
        assert!(jfk_pcm(&stereo).is_none());
    }
}
