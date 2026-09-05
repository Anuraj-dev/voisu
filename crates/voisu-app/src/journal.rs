//! The operator-facing journal lines a finished Recording emits.
//!
//! The first line preserves the historical human-readable message, with control
//! characters escaped so the message cannot end the entry (journald splits the
//! daemon's stderr on `\n`, and the boundary diagnostic carries a subprocess's
//! own stderr — see [`escape_journal_control`]). The second is a stable,
//! timing-only machine record; it never interpolates the diagnostic, so
//! multiline or unusually long boundary output cannot split or displace its
//! keys, nor forge a record of its own.
//!
//! ```text
//! Recording <id>: <message>
//! Recording <id>: outcome=<ok|error> correlation_id=<id> \
//!   first_chunk_ms=<ms> capture_finalized_ms=<ms> \
//!   provider_timings_ms=<provider>:<ms>[,<provider>:<ms>] release_to_text_ms=<ms> \
//!   recording_duration_ms=<ms> stop_to_finalized_ms=<ms> stop_to_delivered_ms=<ms>
//! ```
//!
//! Every structured key is always present. A value Voisu did not measure for
//! that Recording renders as [`ABSENT`] rather than being omitted. The three
//! stop-anchored timings exclude the user's speech duration, unlike the
//! deprecated `release_to_text_ms` (measured from recording start), so latency
//! percentiles computed over them are comparable across dictation lengths.

use voisu_core::{LifecycleEvidence, Provider, ProviderTiming};

/// Rendered in place of any value the Recording did not measure. A key is never
/// dropped: a parser that splits on `key=value` sees a stable key set.
pub const ABSENT: &str = "-";

/// The fixed message of a successful Recording's journal line. The failure path
/// carries the boundary diagnostic here instead.
pub const DELIVERED_MESSAGE: &str = "delivered";

#[derive(Debug, Eq, PartialEq)]
pub struct RecordingJournalLines {
    pub human: String,
    pub structured: String,
}

/// Renders the historical human line and the separate timing-only machine line.
pub fn recording_journal_lines(
    recording_id: u64,
    evidence: &LifecycleEvidence,
    diagnostic: Option<&str>,
) -> RecordingJournalLines {
    let (message, outcome) = match diagnostic {
        Some(diagnostic) => (diagnostic, "error"),
        None => (DELIVERED_MESSAGE, "ok"),
    };
    RecordingJournalLines {
        human: format!(
            "Recording {recording_id}: {}",
            escape_journal_control(message)
        ),
        structured: format!(
            "Recording {recording_id}: outcome={outcome} correlation_id={} \
             first_chunk_ms={} capture_finalized_ms={} provider_timings_ms={} \
             release_to_text_ms={} recording_duration_ms={} stop_to_finalized_ms={} \
             stop_to_delivered_ms={}",
            render_correlation_id(&evidence.correlation_id),
            render_millis(evidence.first_chunk_ms),
            render_millis(evidence.capture_finalized_ms),
            render_provider_timings(&evidence.provider_timings_ms),
            render_millis(evidence.release_to_text_ms),
            render_millis(evidence.recording_duration_ms),
            render_millis(evidence.stop_to_finalized_ms),
            render_millis(evidence.stop_to_delivered_ms),
        ),
    }
}

/// Escapes the control characters that would otherwise let a boundary
/// diagnostic forge a journal entry.
///
/// journald splits the daemon's stderr stream into entries on `\n`, and the
/// boundary diagnostic is not Voisu's text: `capture_process_error` embeds a
/// subprocess's stderr verbatim. A `pw-record` whose stderr contained
/// `\nRecording 7: outcome=ok correlation_id=… release_to_text_ms=…` would
/// therefore produce a journal entry byte-identical to a genuine structured
/// record, and an operator computing percentiles by grepping `outcome=` would
/// silently ingest fabricated data points.
///
/// The message itself is preserved — spec §4 fixes the message text, and
/// escaping a control byte is not changing the message. The escape is lossless
/// and reversible, so the historical detail is still fully readable; it just
/// cannot end the entry any more.
pub fn escape_journal_control(message: &str) -> std::borrow::Cow<'_, str> {
    if !message.chars().any(char::is_control) {
        return std::borrow::Cow::Borrowed(message);
    }
    let mut escaped = String::with_capacity(message.len() + 8);
    for character in message.chars() {
        match character {
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            control if control.is_control() => {
                escaped.push_str(&format!("\\u{{{:04x}}}", control as u32));
            }
            plain => escaped.push(plain),
        }
    }
    std::borrow::Cow::Owned(escaped)
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
            truncated_by: None,
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
            provider_failures: Vec::new(),
            release_to_text_ms: Some(1_311),
            // Stop anchored at 900 ms from start, so the stop-anchored fields
            // are the deprecated start-anchored total minus the speech
            // duration: 1288 − 900 = 388 to settlement, 1311 − 900 = 411 to
            // delivery.
            recording_duration_ms: Some(900),
            stop_to_finalized_ms: Some(388),
            stop_to_delivered_ms: Some(411),
            transcript_selection: None,
            validation_reason: None,
            fallback_reason: None,
            reconciliation_requested: false,
            recovery_attempted: false,
            source_selection_diagnostic: None,
            intent_reconstruction: None,
            confidence_arbitration: None,
        }
    }

    #[test]
    fn a_delivered_recording_renders_human_and_structured_lines() {
        let lines = recording_journal_lines(7, &evidence(), None);
        assert_eq!(lines.human, "Recording 7: delivered");
        assert_eq!(
            lines.structured,
            "Recording 7: outcome=ok correlation_id=rec-4242-7-1700000000000 \
             first_chunk_ms=118 capture_finalized_ms=842 \
             provider_timings_ms=deepgram:412,groq:640 release_to_text_ms=1311 \
             recording_duration_ms=900 stop_to_finalized_ms=388 stop_to_delivered_ms=411"
        );
    }

    #[test]
    fn a_failed_recording_keeps_the_historical_line_and_gains_the_timings() {
        let lines = recording_journal_lines(7, &evidence(), Some("Provider Deadline elapsed"));
        assert_eq!(
            lines.human, "Recording 7: Provider Deadline elapsed",
            "the historical failure line must survive verbatim"
        );
        assert_eq!(
            lines.structured,
            "Recording 7: outcome=error correlation_id=rec-4242-7-1700000000000 \
             first_chunk_ms=118 capture_finalized_ms=842 \
             provider_timings_ms=deepgram:412,groq:640 release_to_text_ms=1311 \
             recording_duration_ms=900 stop_to_finalized_ms=388 stop_to_delivered_ms=411"
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
        evidence.recording_duration_ms = None;
        evidence.stop_to_finalized_ms = None;
        evidence.stop_to_delivered_ms = None;

        assert_eq!(
            recording_journal_lines(9, &evidence, Some("capture ended without audio")),
            RecordingJournalLines {
                human: "Recording 9: capture ended without audio".to_owned(),
                structured: "Recording 9: outcome=error correlation_id=- first_chunk_ms=- \
                             capture_finalized_ms=- provider_timings_ms=- \
                             release_to_text_ms=- recording_duration_ms=- \
                             stop_to_finalized_ms=- stop_to_delivered_ms=-"
                    .to_owned(),
            }
        );
    }

    #[test]
    fn multiline_diagnostic_cannot_split_or_displace_the_structured_line() {
        let lines = recording_journal_lines(
            7,
            &evidence(),
            Some("pw-record failed\nALSA device disappeared"),
        );
        // journald splits the daemon's stderr on `\n`, so a raw newline here
        // would make the boundary detail its own journal entry. The message is
        // unchanged; only the control byte is escaped, losslessly.
        assert_eq!(
            lines.human,
            "Recording 7: pw-record failed\\nALSA device disappeared"
        );
        assert!(
            !lines.human.contains('\n'),
            "the human line must stay one journal entry: {}",
            lines.human
        );
        assert_eq!(
            lines.structured,
            "Recording 7: outcome=error correlation_id=rec-4242-7-1700000000000 \
             first_chunk_ms=118 capture_finalized_ms=842 \
             provider_timings_ms=deepgram:412,groq:640 release_to_text_ms=1311 \
             recording_duration_ms=900 stop_to_finalized_ms=388 stop_to_delivered_ms=411"
        );
        assert!(!lines.structured.contains("pw-record"));
        assert!(!lines.structured.contains('\n'));
    }

    #[test]
    fn a_diagnostic_cannot_forge_a_structured_journal_line() {
        // `capture_process_error` embeds a subprocess's stderr verbatim in the
        // boundary diagnostic. journald splits stderr on `\n`, so an embedded
        // newline does not wrap the human line — it STARTS A NEW JOURNAL ENTRY
        // whose text the subprocess fully controls. An operator, or a script
        // computing percentiles by grepping `outcome=`, would then ingest a
        // fabricated data point it cannot tell from a genuine record.
        let forged = "Recording 7: outcome=ok correlation_id=rec-forged \
                      first_chunk_ms=1 capture_finalized_ms=1 \
                      provider_timings_ms=deepgram:1 release_to_text_ms=1";
        let lines =
            recording_journal_lines(7, &evidence(), Some(&format!("pw-record failed\n{forged}")));

        assert_eq!(
            lines.human.lines().count(),
            1,
            "a diagnostic must never produce a second journal entry: {}",
            lines.human
        );
        assert!(
            !lines.human.contains('\n') && !lines.human.contains('\r'),
            "no entry separator may survive into the human line: {}",
            lines.human
        );
        // The forged text stays readable — it is escaped, not censored — but it
        // is now unambiguously part of the human line's message.
        assert_eq!(
            lines.human,
            format!("Recording 7: pw-record failed\\n{forged}")
        );
        assert!(
            lines
                .human
                .starts_with("Recording 7: pw-record failed\\nRecording 7: outcome=ok"),
            "{}",
            lines.human
        );
    }

    #[test]
    fn every_control_character_in_a_diagnostic_is_escaped_onto_one_line() {
        let lines =
            recording_journal_lines(3, &evidence(), Some("carriage\rtab\tnull\u{0}bell\u{7}"));
        assert_eq!(
            lines.human,
            "Recording 3: carriage\\rtab\\tnull\\u{0000}bell\\u{0007}"
        );
        assert!(
            !lines.human.chars().any(char::is_control),
            "no control byte may reach the journal: {:?}",
            lines.human
        );
    }

    #[test]
    fn a_diagnostic_without_control_characters_is_passed_through_untouched() {
        let lines = recording_journal_lines(4, &evidence(), Some(r"C:\path\not\escaped"));
        assert_eq!(
            lines.human, r"Recording 4: C:\path\not\escaped",
            "escaping must not rewrite backslashes that are already literal text"
        );
    }

    #[test]
    fn every_key_appears_exactly_once_on_both_paths() {
        // The line is a parseable key=value record: a duplicated key would make
        // percentile extraction ambiguous, and a missing one would make the
        // field set depend on the outcome.
        const KEYS: [&str; 9] = [
            "outcome=",
            "correlation_id=",
            "first_chunk_ms=",
            "capture_finalized_ms=",
            "provider_timings_ms=",
            "release_to_text_ms=",
            "recording_duration_ms=",
            "stop_to_finalized_ms=",
            "stop_to_delivered_ms=",
        ];
        for diagnostic in [None, Some("boundary failed")] {
            let lines = recording_journal_lines(1, &evidence(), diagnostic);
            for key in KEYS {
                assert_eq!(
                    lines.structured.matches(key).count(),
                    1,
                    "{key} must appear exactly once in {}",
                    lines.structured
                );
                assert!(
                    !lines.human.contains(key),
                    "the human line must not carry structured keys: {}",
                    lines.human
                );
            }
            assert!(!lines.structured.contains('\n'));
        }
    }
}
