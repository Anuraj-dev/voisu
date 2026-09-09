//! Local status presentation from structured daemon fields.
//!
//! Reads `AsrModeStatus` only. Does not load models or check Cloud keys.

use voisu_core::{ASR_MODE_V1, AsrMode, AsrModeStatus, LocalReadiness};

/// Historic first line, then pending/active/revision/readiness/capability.
pub fn write_cli_status(message: &str, asr: Option<&AsrModeStatus>) {
    print!("{}", format_cli_status(message, asr));
}

#[must_use]
pub fn format_cli_status(message: &str, asr: Option<&AsrModeStatus>) -> String {
    let mut out = String::new();
    out.push_str(message);
    out.push('\n');
    let Some(asr) = asr else {
        return out;
    };
    match asr.pending {
        Some(mode) => {
            out.push_str("asr mode pending: ");
            out.push_str(mode.as_str());
            out.push('\n');
        }
        None => out.push_str("asr mode pending: unknown\n"),
    }
    if let Some(active) = asr.active {
        out.push_str("asr mode active: ");
        out.push_str(active.as_str());
        out.push('\n');
    }
    match asr.revision {
        Some(revision) => {
            out.push_str("config revision: ");
            out.push_str(&revision.to_string());
            out.push('\n');
        }
        None => out.push_str("config revision: unknown\n"),
    }
    out.push_str("local readiness: ");
    out.push_str(&format_local_readiness(&asr.local_readiness));
    out.push('\n');
    if let Some(identity) = model_identity(&asr.local_readiness) {
        out.push_str("model identity: ");
        out.push_str(identity);
        out.push('\n');
    }
    if let Some(error) = &asr.admission_error {
        out.push_str("asr admission: ");
        out.push_str(error);
        out.push('\n');
    }
    if let Some(retention) = &asr.audio_retention {
        out.push_str("audio retention: ");
        out.push_str(retention);
        out.push('\n');
    }
    out.push_str("asr capability: ");
    out.push_str(ASR_MODE_V1);
    out.push('\n');
    if let Some(line) = recording_path_line(asr) {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

#[must_use]
pub fn format_local_readiness(readiness: &LocalReadiness) -> String {
    match readiness {
        LocalReadiness::Absent => "absent".to_owned(),
        LocalReadiness::Verifying => "verifying".to_owned(),
        LocalReadiness::Loading => "loading".to_owned(),
        LocalReadiness::Ready {
            model_identity: None,
        } => "ready".to_owned(),
        LocalReadiness::Ready {
            model_identity: Some(identity),
        } => format!("ready ({identity})"),
        LocalReadiness::Busy => "busy".to_owned(),
        LocalReadiness::Stopping => "stopping".to_owned(),
        LocalReadiness::Unavailable { error } => format!("unavailable ({error})"),
    }
}

#[must_use]
pub fn model_identity(readiness: &LocalReadiness) -> Option<&str> {
    match readiness {
        LocalReadiness::Ready {
            model_identity: Some(identity),
        } => Some(identity.as_str()),
        _ => None,
    }
}

/// A pending Local selection during a Cloud Recording does not make that
/// Recording offline.
#[must_use]
pub fn recording_is_offline(asr: &AsrModeStatus) -> bool {
    asr.active == Some(AsrMode::Local)
}

#[must_use]
pub fn local_selected_model_unavailable(asr: &AsrModeStatus) -> bool {
    asr.pending == Some(AsrMode::Local)
        && matches!(asr.local_readiness, LocalReadiness::Unavailable { .. })
}

#[must_use]
pub fn local_ready(asr: &AsrModeStatus) -> bool {
    asr.pending == Some(AsrMode::Local) && asr.local_readiness.is_ready()
}

fn recording_path_line(asr: &AsrModeStatus) -> Option<String> {
    let pending = asr.pending?;
    let active = asr.active?;
    if pending == active {
        return None;
    }
    if active == AsrMode::Cloud && pending == AsrMode::Local {
        return Some(
            "recording path: cloud (this Recording is not offline; Local is pending)".to_owned(),
        );
    }
    Some(format!(
        "recording path: {} (pending {})",
        active.as_str(),
        pending.as_str()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(
        pending: Option<AsrMode>,
        active: Option<AsrMode>,
        readiness: LocalReadiness,
    ) -> AsrModeStatus {
        AsrModeStatus {
            pending,
            active,
            revision: Some(7),
            local_readiness: readiness,
            admission_error: None,
            audio_retention: None,
            local_path_clean: None,
        }
    }

    #[test]
    fn distinguishes_local_selected_unavailable_from_ready() {
        let unavailable = status(
            Some(AsrMode::Local),
            None,
            LocalReadiness::Unavailable {
                error: "Local selected; model unavailable".into(),
            },
        );
        let ready = status(
            Some(AsrMode::Local),
            None,
            LocalReadiness::Ready {
                model_identity: Some("l3-health-fixture".into()),
            },
        );
        let unavailable_text = format_cli_status("idle", Some(&unavailable));
        let ready_text = format_cli_status("idle", Some(&ready));
        assert!(
            unavailable_text
                .contains("local readiness: unavailable (Local selected; model unavailable)"),
            "{unavailable_text}"
        );
        assert!(local_selected_model_unavailable(&unavailable));
        assert!(!local_ready(&unavailable));
        assert!(ready_text.contains("local readiness: ready (l3-health-fixture)"));
        assert!(ready_text.contains("model identity: l3-health-fixture"));
        assert!(local_ready(&ready));
        assert!(!local_selected_model_unavailable(&ready));
        assert!(unavailable_text.contains("asr capability: asr-mode-v1"));
        assert!(unavailable_text.contains("config revision: 7"));
        assert!(unavailable_text.contains("asr mode pending: local"));
    }

    #[test]
    fn pending_local_during_cloud_recording_is_not_offline() {
        let asr = status(
            Some(AsrMode::Local),
            Some(AsrMode::Cloud),
            LocalReadiness::Unavailable {
                error: "Local selected; model unavailable".into(),
            },
        );
        assert!(!recording_is_offline(&asr));
        let text = format_cli_status("Recording", Some(&asr));
        assert!(text.contains("asr mode pending: local"), "{text}");
        assert!(text.contains("asr mode active: cloud"), "{text}");
        assert!(
            text.contains(
                "recording path: cloud (this Recording is not offline; Local is pending)"
            ),
            "{text}"
        );
    }

    #[test]
    fn missing_structured_status_does_not_infer_cloud() {
        let text = format_cli_status("idle", None);
        assert_eq!(text, "idle\n");
        assert!(!text.contains("cloud"));
        assert!(!text.contains("asr-mode-v1"));
    }
}
