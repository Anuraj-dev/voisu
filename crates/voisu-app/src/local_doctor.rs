//! Local doctor presentation. Skips Cloud probes rather than hiding them.

use voisu_core::{AsrMode, AsrModeStatus, LocalReadiness, ReadinessStatus};

use crate::local_status::{format_local_readiness, recording_is_offline};

/// Banner for the explicitly invoked Cloud credential-maintenance command.
pub const CLOUD_CREDENTIAL_MAINTENANCE: &str =
    "Cloud credential-maintenance: explicitly invoked; never run from Local setup or doctor";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoctorCheck {
    pub label: String,
    pub value: String,
    pub status: ReadinessStatus,
    pub detail: String,
    pub action: Option<String>,
}

#[must_use]
pub fn skip_cloud_probes() -> bool {
    crate::local_routing::skip_cloud_doctor_probes()
}

/// Structured Local rows from daemon status. Empty when Local is not selected.
#[must_use]
pub fn rows_from_status(asr: &AsrModeStatus) -> Vec<DoctorCheck> {
    if asr.pending != Some(AsrMode::Local) && asr.active != Some(AsrMode::Local) {
        return Vec::new();
    }
    let mut rows = vec![
        DoctorCheck {
            label: "ASR pending".into(),
            value: asr
                .pending
                .map(AsrMode::as_str)
                .unwrap_or("unknown")
                .to_owned(),
            status: ReadinessStatus::Pass,
            detail: "pending ASR mode from structured status; models are not loaded".into(),
            action: None,
        },
        DoctorCheck {
            label: "ASR active".into(),
            value: asr.active.map(AsrMode::as_str).unwrap_or("none").to_owned(),
            status: ReadinessStatus::Pass,
            detail: if asr.active == Some(AsrMode::Cloud) && asr.pending == Some(AsrMode::Local) {
                "active Cloud Recording is not offline; Local is pending".into()
            } else {
                "active Recording mode when present".into()
            },
            action: None,
        },
        local_model_row(&asr.local_readiness),
    ];
    if let Some(revision) = asr.revision {
        rows.push(DoctorCheck {
            label: "ASR revision".into(),
            value: revision.to_string(),
            status: ReadinessStatus::Pass,
            detail: "config revision from structured status".into(),
            action: None,
        });
    }
    if asr.active == Some(AsrMode::Cloud) && asr.pending == Some(AsrMode::Local) {
        rows.push(DoctorCheck {
            label: "Recording".into(),
            value: "cloud".into(),
            status: ReadinessStatus::Pass,
            detail: "pending Local does not make this Recording offline".into(),
            action: None,
        });
        assert!(!recording_is_offline(asr));
    }
    rows
}

fn local_model_row(readiness: &LocalReadiness) -> DoctorCheck {
    match readiness {
        LocalReadiness::Ready {
            model_identity: Some(identity),
        } => DoctorCheck {
            label: "Local model".into(),
            value: identity.clone(),
            status: ReadinessStatus::Pass,
            detail: "Local ready".into(),
            action: None,
        },
        LocalReadiness::Ready {
            model_identity: None,
        } => DoctorCheck {
            label: "Local model".into(),
            value: "ready".into(),
            status: ReadinessStatus::Pass,
            detail: "Local ready".into(),
            action: None,
        },
        LocalReadiness::Loading | LocalReadiness::Verifying => DoctorCheck {
            label: "Local model".into(),
            value: format_local_readiness(readiness),
            status: ReadinessStatus::Warn,
            detail: "Local selected; model loading; Start is refused until ready".into(),
            action: Some("wait for ready, then trigger again".into()),
        },
        LocalReadiness::Unavailable { error } => DoctorCheck {
            label: "Local model".into(),
            value: "unavailable".into(),
            status: ReadinessStatus::Fail,
            detail: error.clone(),
            action: Some("run `voisu setup` to install or restore a Local model".into()),
        },
        other => DoctorCheck {
            label: "Local model".into(),
            value: format_local_readiness(other),
            status: ReadinessStatus::Warn,
            detail: "Local worker is not idle-ready".into(),
            action: None,
        },
    }
}

/// Explicit SKIP rows so Cloud probes are not silently omitted.
#[must_use]
pub fn skipped_cloud_key_rows() -> Vec<DoctorCheck> {
    ["Deepgram key", "Groq key"]
        .into_iter()
        .map(|label| DoctorCheck {
            label: label.into(),
            value: "not probed".into(),
            status: ReadinessStatus::Skip,
            detail: "Local doctor skips Cloud credential probes rather than hiding their results"
                .into(),
            action: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_probes_are_skipped_not_hidden() {
        let rows = skipped_cloud_key_rows();
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .all(|row| row.status == ReadinessStatus::Skip && row.value == "not probed")
        );
        assert!(
            rows.iter()
                .any(|row| row.label == "Deepgram key" && row.detail.contains("skips Cloud"))
        );
        assert!(
            rows.iter()
                .any(|row| row.label == "Groq key" && row.detail.contains("skips Cloud"))
        );
    }

    #[test]
    fn local_rows_distinguish_unavailable_from_ready() {
        let unavailable = AsrModeStatus {
            pending: Some(AsrMode::Local),
            active: None,
            revision: Some(1),
            local_readiness: LocalReadiness::Unavailable {
                error: "Local selected; model unavailable".into(),
            },
            admission_error: None,
            audio_retention: None,
            local_path_clean: None,
        };
        let ready = AsrModeStatus {
            pending: Some(AsrMode::Local),
            active: None,
            revision: Some(1),
            local_readiness: LocalReadiness::Ready {
                model_identity: Some("l3-health-fixture".into()),
            },
            admission_error: None,
            audio_retention: None,
            local_path_clean: None,
        };
        let unavailable_rows = rows_from_status(&unavailable);
        let ready_rows = rows_from_status(&ready);
        assert!(unavailable_rows.iter().any(|row| row.label == "Local model"
            && row.status == ReadinessStatus::Fail
            && row.detail.contains("model unavailable")));
        assert!(ready_rows.iter().any(|row| row.label == "Local model"
            && row.status == ReadinessStatus::Pass
            && row.value == "l3-health-fixture"));
    }

    #[test]
    fn cloud_pending_status_adds_no_local_rows() {
        let cloud = AsrModeStatus {
            pending: Some(AsrMode::Cloud),
            active: None,
            revision: Some(1),
            local_readiness: LocalReadiness::Absent,
            admission_error: None,
            audio_retention: None,
            local_path_clean: None,
        };
        assert!(rows_from_status(&cloud).is_empty());
    }

    #[test]
    fn pending_local_during_cloud_recording_is_named() {
        let asr = AsrModeStatus {
            pending: Some(AsrMode::Local),
            active: Some(AsrMode::Cloud),
            revision: Some(2),
            local_readiness: LocalReadiness::Loading,
            admission_error: None,
            audio_retention: None,
            local_path_clean: None,
        };
        let rows = rows_from_status(&asr);
        assert!(
            rows.iter()
                .any(|row| row.label == "Recording" && row.value == "cloud")
        );
        assert!(!recording_is_offline(&asr));
    }

    #[test]
    fn cloud_credential_maintenance_is_named_and_not_a_doctor_action() {
        assert!(CLOUD_CREDENTIAL_MAINTENANCE.contains("Cloud credential-maintenance"));
        assert!(CLOUD_CREDENTIAL_MAINTENANCE.contains("never run from Local setup or doctor"));
        assert!(!skipped_cloud_key_rows().iter().any(|row| {
            row.action
                .as_deref()
                .is_some_and(|action| action.contains("auth verify"))
        }));
    }
}
