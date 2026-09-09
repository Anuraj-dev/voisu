//! Cloud-owned grammar and DPR construction.
//!
//! These clients are built only after Cloud admission. Local Start/Replay,
//! startup, and prewarm never call this module.

use crate::dpr_cloud::DprCloudClient;
use crate::grammar_http::GrammarHttpClient;
use crate::local_worker::CloudCapabilitySentinel;
use crate::minimal_grammar::MinimalGrammarAdapter;
use voisu_core::AsrMode;

/// Cloud-only formatting clients. Absence means they were not constructed.
#[derive(Clone, Debug)]
pub struct CloudOwnedResources {
    pub grammar: Option<MinimalGrammarAdapter>,
    pub dpr: Option<DprCloudClient>,
}

impl CloudOwnedResources {
    #[must_use]
    pub fn empty() -> Self {
        Self {
            grammar: None,
            dpr: None,
        }
    }
}

/// Construct grammar/DPR only for an admitted Cloud Recording or Replay.
pub fn construct_for_cloud(
    mode: AsrMode,
    controlled: bool,
    dpr_enabled: bool,
    qwen_format_enabled: bool,
    sentinel: Option<&mut CloudCapabilitySentinel>,
) -> CloudOwnedResources {
    if mode != AsrMode::Cloud {
        return CloudOwnedResources::empty();
    }
    crate::local_routing::note_cloud_capability_used();
    if let Some(sentinel) = sentinel {
        sentinel.construct_cloud_client();
    }
    let grammar = if controlled {
        std::env::var("VOISU_TEST_MINIMAL_GRAMMAR_ENDPOINT")
            .ok()
            .and_then(|endpoint| GrammarHttpClient::with_endpoint(endpoint).ok())
            .map(MinimalGrammarAdapter::new)
    } else {
        match MinimalGrammarAdapter::production() {
            Ok(adapter) => Some(adapter),
            Err(error) => {
                eprintln!("Minimal Grammar unavailable: {error}");
                None
            }
        }
    };
    let dpr = if !dpr_enabled || !qwen_format_enabled {
        None
    } else if controlled {
        std::env::var("VOISU_TEST_DPR_ENDPOINT")
            .ok()
            .and_then(|endpoint| DprCloudClient::with_endpoint(endpoint).ok())
    } else {
        match DprCloudClient::groq() {
            Ok(client) => Some(client),
            Err(error) => {
                eprintln!("Developer Prompt Rendering cloud unavailable: {error}");
                None
            }
        }
    };
    CloudOwnedResources { grammar, dpr }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_admission_does_not_construct_cloud_clients() {
        let mut sentinel = CloudCapabilitySentinel::new();
        let owned = construct_for_cloud(AsrMode::Local, true, true, true, Some(&mut sentinel));
        assert!(owned.grammar.is_none());
        assert!(owned.dpr.is_none());
        assert!(sentinel.local_path_clean());
    }

    #[test]
    fn cloud_admission_records_capability_construction() {
        let mut sentinel = CloudCapabilitySentinel::new();
        let _ = construct_for_cloud(AsrMode::Cloud, true, false, false, Some(&mut sentinel));
        assert!(!sentinel.local_path_clean());
    }
}
