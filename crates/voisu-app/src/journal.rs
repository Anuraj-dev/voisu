//! The operator-facing journal line a finished Recording emits.
//!
//! Voisu already measures `first_chunk_ms`, `capture_finalized_ms`,
//! `provider_timings_ms`, and `release_to_text_ms` for every Recording and
//! carries them into the local diagnostic record. The journal — which survives
//! restarts, updates, and the diagnostic ring's retention bound — saw none of
//! them: the success path logged nothing at all and the failure path logged only
//! its boundary diagnostic. This module renders those already-measured values as
//! one stable, machine-parseable line so an operator can compute percentiles
//! from `journalctl` output without hand-editing log text.
//!
//! Shape, identical on both paths:
//!
//! ```text
//! Recording <id>: <message> outcome=<ok|error> correlation_id=<id> \
//!   first_chunk_ms=<ms> capture_finalized_ms=<ms> \
//!   provider_timings_ms=<provider>:<ms>[,<provider>:<ms>] release_to_text_ms=<ms>
//! ```
//!
//! The failure path's `<message>` is EXACTLY the boundary diagnostic the daemon
//! has always printed, and the `Recording <id>: <diagnostic>` prefix is
//! byte-identical to the historical line — a fourteen-day journal corpus parses
//! the same way, and only gains trailing `key=value` pairs. The success path,
//! which previously printed nothing, uses the fixed message `delivered`.
//!
//! Every key is always present. A value Voisu did not measure for that Recording
//! renders as [`ABSENT`] rather than being omitted, so a parser can rely on the
//! key set without inspecting the outcome.

use voisu_core::{LifecycleEvidence, Provider, ProviderTiming};

/// Rendered in place of any value the Recording did not measure. A key is never
/// dropped: a parser that splits on `key=value` sees a stable key set.
pub const ABSENT: &str = "-";

/// The fixed message of a successful Recording's journal line. The failure path
/// carries the boundary diagnostic here instead.
pub const DELIVERED_MESSAGE: &str = "delivered";

/// Renders one Recording's journal line. `diagnostic` is `None` for a Recording
/// that delivered and `Some(boundary_diagnostic)` for one that failed; the
/// failure text is emitted verbatim so the historical line shape is preserved.
pub fn recording_journal_line(
    recording_id: u64,
    evidence: &LifecycleEvidence,
    diagnostic: Option<&str>,
) -> String {
    let (message, outcome) = match diagnostic {
        Some(diagnostic) => (diagnostic, "error"),
        None => (DELIVERED_MESSAGE, "ok"),
    };
    format!(
        "Recording {recording_id}: {message} outcome={outcome} correlation_id={} \
         first_chunk_ms={} capture_finalized_ms={} provider_timings_ms={} \
         release_to_text_ms={}",
        render_correlation_id(&evidence.correlation_id),
        render_millis(evidence.first_chunk_ms),
        render_millis(evidence.capture_finalized_ms),
        render_provider_timings(&evidence.provider_timings_ms),
        render_millis(evidence.release_to_text_ms),
    )
}

/// A correlation ID is daemon-generated and space-free, but an empty one (an
/// older record, or a Recording that failed before correlation) must still leave
/// the key parseable rather than emitting a bare `correlation_id=`.
fn render_correlation_id(correlation_id: &str) -> &str {
    if correlation_id.is_empty() {
        ABSENT
    } else {
        correlation_id
    }
}

fn render_millis(value: Option<u64>) -> String {
    match value {
        Some(value) => value.to_string(),
        None => ABSENT.to_owned(),
    }
}

/// `deepgram:412,groq:640` — comma-separated, whitespace-free, in the order the
/// providers completed. No timing at all renders as [`ABSENT`].
fn render_provider_timings(timings: &[ProviderTiming]) -> String {
    if timings.is_empty() {
        return ABSENT.to_owned();
    }
    timings
        .iter()
        .map(|timing| format!("{}:{}", provider_key(timing.provider), timing.completed_ms))
        .collect::<Vec<_>>()
        .join(",")
}

/// A lowercase, stable key for a provider. Deliberately not `cli_label`, which
/// is display text and may be re-cased for humans.
fn provider_key(provider: Provider) -> &'static str {
    match provider {
        Provider::Deepgram => "deepgram",
        Provider::Groq => "groq",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use voisu_core::LifecycleStage;

    fn evidence() -> LifecycleEvidence {
        LifecycleEvidence {
            recording_id: 7,
            correlation_id: "rec-4242-7-1700000000000".to_owned(),
            stages: vec![LifecycleStage::CaptureStarted],
            delivery_count: 1,
            delivery_method: None,
            delivery_fallback_reason: None,
            streamed_chunk_count: 3,
            source_transcript_providers: Vec::new(),
            first_chunk_ms: Some(118),
            capture_finalized_ms: Some(842),
            provider_timings_ms: vec![
                ProviderTiming {
                    provider: Provider::Deepgram,
                    completed_ms: 412,
                },
                ProviderTiming {
                    provider: Provider::Groq,
                    completed_ms: 640,
                },
            ],
            release_to_text_ms: Some(1_311),
            transcript_selection: None,
            validation_reason: None,
            fallback_reason: None,
            reconciliation_requested: false,
            recovery_attempted: false,
        }
    }

    #[test]
    fn a_delivered_recording_renders_every_measured_stage_as_key_values() {
        assert_eq!(
            recording_journal_line(7, &evidence(), None),
            "Recording 7: delivered outcome=ok correlation_id=rec-4242-7-1700000000000 \
             first_chunk_ms=118 capture_finalized_ms=842 \
             provider_timings_ms=deepgram:412,groq:640 release_to_text_ms=1311"
        );
    }

    #[test]
    fn a_failed_recording_keeps_the_historical_prefix_and_gains_the_timings() {
        // The fourteen-day journal corpus parses `Recording <id>: <diagnostic>`.
        // That prefix must stay byte-identical; the timings are appended only.
        let line = recording_journal_line(7, &evidence(), Some("Provider Deadline elapsed"));
        assert!(
            line.starts_with("Recording 7: Provider Deadline elapsed"),
            "the historical failure line must survive verbatim as a prefix: {line}"
        );
        assert_eq!(
            line,
            "Recording 7: Provider Deadline elapsed outcome=error \
             correlation_id=rec-4242-7-1700000000000 first_chunk_ms=118 \
             capture_finalized_ms=842 provider_timings_ms=deepgram:412,groq:640 \
             release_to_text_ms=1311"
        );
    }

    #[test]
    fn unmeasured_stages_keep_their_keys_with_the_absent_marker() {
        let mut evidence = evidence();
        evidence.correlation_id = String::new();
        evidence.first_chunk_ms = None;
        evidence.capture_finalized_ms = None;
        evidence.provider_timings_ms = Vec::new();
        evidence.release_to_text_ms = None;

        assert_eq!(
            recording_journal_line(9, &evidence, Some("capture ended without audio")),
            "Recording 9: capture ended without audio outcome=error correlation_id=- \
             first_chunk_ms=- capture_finalized_ms=- provider_timings_ms=- \
             release_to_text_ms=-"
        );
    }

    #[test]
    fn every_key_appears_exactly_once_on_both_paths() {
        // The line is a parseable key=value record: a duplicated key would make
        // percentile extraction ambiguous, and a missing one would make the
        // field set depend on the outcome.
        const KEYS: [&str; 6] = [
            "outcome=",
            "correlation_id=",
            "first_chunk_ms=",
            "capture_finalized_ms=",
            "provider_timings_ms=",
            "release_to_text_ms=",
        ];
        for diagnostic in [None, Some("boundary failed")] {
            let line = recording_journal_line(1, &evidence(), diagnostic);
            for key in KEYS {
                assert_eq!(
                    line.matches(key).count(),
                    1,
                    "{key} must appear exactly once in {line}"
                );
            }
            assert!(
                !line.contains('\n'),
                "a journal record must be a single line: {line}"
            );
        }
    }
}
