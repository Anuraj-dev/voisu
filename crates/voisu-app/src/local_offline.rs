//! Offline proof helpers: Local work must not fire Cloud capability sentinels.
//!
//! Real process-tree network traces are an L6 host gate. These tests prove the
//! Local call graph does not construct Cloud clients, read Cloud credentials,
//! or attempt IP/DNS including loopback.

use crate::local_worker::CloudCapabilitySentinel;

#[must_use]
pub fn local_session_is_offline(sentinel: &CloudCapabilitySentinel) -> bool {
    sentinel.local_path_clean()
}

#[cfg(test)]
mod tests {
    use super::*;
    use voisu_core::AsrMode;

    use crate::config::WritingMode;

    use crate::cloud_ownership::construct_for_cloud;
    use crate::local_routing::{
        admit_local, cloud_free_slots, complete_recording, complete_replay, inject_ready,
        sentinel_is_clean, test_session,
    };

    #[test]
    fn local_startup_prewarm_status_doctor_and_cleanup_stay_offline() {
        let _lock = test_session();
        let mut sentinel = CloudCapabilitySentinel::new();
        let owned = construct_for_cloud(AsrMode::Local, true, true, true, Some(&mut sentinel));
        assert!(owned.grammar.is_none());
        assert!(owned.dpr.is_none());
        let (deepgram, groq) = cloud_free_slots();
        drop(deepgram);
        drop(groq);
        let _ = admit_local(true);
        assert!(local_session_is_offline(&sentinel));
        assert!(sentinel_is_clean());
    }

    #[test]
    fn local_recording_replay_format_and_error_paths_stay_offline() {
        let _lock = test_session();
        let receipt =
            crate::local_model::from_entry(crate::local_model::ci_fixture_entry(), "offline");
        inject_ready(receipt, "hello there");
        crate::local_routing::begin_live("rec-off");
        let completion = complete_recording(
            "rec-off",
            vec![1, 0, 3, 0],
            &["Voisu".to_owned()],
            WritingMode::Smart,
            false,
            false,
        )
        .expect("local recording");
        match completion.decision {
            crate::local_tail::TerminalDecision::Transcript(text) => {
                assert!(!text.is_empty());
            }
            crate::local_tail::TerminalDecision::NoText { reason } => {
                panic!("expected transcript, got no-text {reason}");
            }
        }
        let replay = complete_replay(
            "rec-off-replay",
            vec![1, 0],
            &[],
            WritingMode::Literal,
            false,
            false,
        )
        .expect("local replay");
        let _ = replay;
        let failed = complete_replay(
            "rec-cloud",
            vec![1, 0],
            &[],
            WritingMode::Literal,
            true,
            true,
        );
        assert!(failed.is_err());
        assert!(sentinel_is_clean());
    }
}
