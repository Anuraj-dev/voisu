//! Local status presentation from structured daemon fields.
//!
//! Reads `AsrModeStatus` only. Does not load models or check Cloud keys.

use voisu_core::{AsrMode, AsrModeStatus, LocalReadiness};

use crate::asr_mode::engine_name;

/// The state line, then the engine, the Local model, and any problem.
pub fn write_cli_status(message: &str, asr: Option<&AsrModeStatus>) {
    print!("{}", format_cli_status(message, asr));
}

/// The daemon reports its state with stable wire labels; people read these.
fn state_line(message: &str) -> &str {
    match message {
        "idle" => "Ready",
        "processing" => "Working on your Transcript",
        other => other,
    }
}

#[must_use]
pub fn format_cli_status(message: &str, asr: Option<&AsrModeStatus>) -> String {
    let mut out = String::new();
    out.push_str(state_line(message));
    out.push('\n');
    let Some(asr) = asr else {
        return out;
    };
    out.push_str("Engine: ");
    match (asr.pending, asr.active) {
        (Some(pending), Some(active)) if pending != active => {
            out.push_str(engine_name(pending));
            out.push_str(" (starts with your next Recording)");
        }
        (Some(pending), _) => out.push_str(engine_name(pending)),
        (None, _) => out.push_str("unknown"),
    }
    out.push('\n');
    out.push_str("Local model: ");
    out.push_str(&format_local_readiness(&asr.local_readiness));
    out.push('\n');
    if let Some(identity) = model_identity(&asr.local_readiness) {
        out.push_str("Model: ");
        out.push_str(identity);
        out.push('\n');
    }
    if let Some(error) = &asr.admission_error {
        out.push_str("Can't switch engine: ");
        out.push_str(error);
        out.push('\n');
    }
    if let Some(retention) = &asr.audio_retention {
        out.push_str("Saved audio: ");
        out.push_str(retention);
        out.push('\n');
    }
    if let Some(line) = recording_path_line(asr) {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

#[must_use]
pub fn format_local_readiness(readiness: &LocalReadiness) -> String {
    match readiness {
        LocalReadiness::Absent => "not installed".to_owned(),
        LocalReadiness::Verifying => "checking".to_owned(),
        LocalReadiness::Loading => "loading".to_owned(),
        LocalReadiness::Ready {
            model_identity: None,
        } => "ready".to_owned(),
        LocalReadiness::Ready {
            model_identity: Some(identity),
        } => format!("ready ({identity})"),
        LocalReadiness::Busy => "busy".to_owned(),
        LocalReadiness::Stopping => "shutting down".to_owned(),
        LocalReadiness::Unavailable { error } => format!("can't load ({error})"),
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
    Some(format!(
        "This Recording uses {}; {} starts with the next one",
        engine_name(active),
        engine_name(pending)
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
                .contains("Local model: can't load (Local selected; model unavailable)"),
            "{unavailable_text}"
        );
        assert!(local_selected_model_unavailable(&unavailable));
        assert!(!local_ready(&unavailable));
        assert!(ready_text.contains("Local model: ready (l3-health-fixture)"));
        assert!(ready_text.contains("Model: l3-health-fixture"));
        assert!(local_ready(&ready));
        assert!(!local_selected_model_unavailable(&ready));
        assert!(unavailable_text.starts_with("Ready\nEngine: Local\n"));
        assert!(!unavailable_text.contains("asr-mode-v1"));
        assert!(!unavailable_text.contains("revision"));
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
        assert!(
            text.contains("Engine: Local (starts with your next Recording)"),
            "{text}"
        );
        assert!(
            text.contains("This Recording uses Cloud; Local starts with the next one"),
            "{text}"
        );
    }

    #[test]
    fn missing_structured_status_does_not_infer_cloud() {
        let text = format_cli_status("idle", None);
        assert_eq!(text, "Ready\n");
        assert!(!text.to_lowercase().contains("cloud"));
        assert!(!text.contains("asr-mode-v1"));
    }

    #[test]
    fn wire_state_labels_read_as_plain_words() {
        assert_eq!(format_cli_status("idle", None), "Ready\n");
        assert_eq!(
            format_cli_status("processing", None),
            "Working on your Transcript\n"
        );
        assert_eq!(format_cli_status("Recording", None), "Recording\n");
    }
}
