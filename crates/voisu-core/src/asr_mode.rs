//! Typed ASR mode, Local readiness, and additive IPC status.

use serde::{Deserialize, Serialize};

/// Protocol 1 capability token for explicit Local/Cloud selection.
pub const ASR_MODE_V1: &str = "asr-mode-v1";

/// User-selected ASR path. Cloud and Local are explicit; there is no fallback.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AsrMode {
    Cloud,
    Local,
}

impl AsrMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cloud => "cloud",
            Self::Local => "local",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "cloud" => Some(Self::Cloud),
            "local" => Some(Self::Local),
            _ => None,
        }
    }
}

/// Observed Local worker readiness. Production Local stays unavailable until
/// later phases install a model and worker.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LocalReadiness {
    Absent,
    Verifying,
    Loading,
    Ready {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_identity: Option<String>,
    },
    Busy,
    Stopping,
    Unavailable {
        error: String,
    },
}

impl LocalReadiness {
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready { .. })
    }

    pub fn admits_capture(&self) -> bool {
        self.is_ready()
    }
}

/// Additive status published by a capable daemon. Missing fields mean
/// unsupported/unknown, never inferred Cloud.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AsrModeStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<AsrMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<AsrMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    pub local_readiness: LocalReadiness,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission_error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Command, PROTOCOL_VERSION, Request, Response};

    #[test]
    fn set_asr_mode_round_trips_on_the_wire() {
        let request = Request::new(Command::SetAsrMode(AsrMode::Local));
        let encoded = serde_json::to_string(&request).unwrap();
        assert!(encoded.contains("set_asr_mode"), "{encoded}");
        assert!(encoded.contains("local"), "{encoded}");
        let decoded: Request = serde_json::from_str(&encoded).unwrap();
        assert!(matches!(
            decoded.command,
            Command::SetAsrMode(AsrMode::Local)
        ));
    }

    #[test]
    fn old_status_json_does_not_infer_cloud() {
        let frame = format!(
            r#"{{"version":{PROTOCOL_VERSION},"ok":true,"state":"idle","message":"idle"}}"#
        );
        let response: Response = serde_json::from_str(&frame).unwrap();
        assert!(response.capabilities.is_empty());
        assert!(response.asr_mode.is_none());
    }

    #[test]
    fn local_unavailable_is_not_ready() {
        let readiness = LocalReadiness::Unavailable {
            error: "local ASR is unavailable".to_owned(),
        };
        assert!(!readiness.admits_capture());
        assert!(!readiness.is_ready());
        assert!(
            LocalReadiness::Ready {
                model_identity: None
            }
            .admits_capture()
        );
    }
}
