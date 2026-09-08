//! Stop-anchored telemetry must exclude speaking time.
//!
//! Controlled Instants: a 2 s+ speech interval is the Recording, not the
//! stop-to-text latency. This test fails if `stop_to_finalized_ms` or
//! `stop_to_delivered_ms` were measured from Recording start.

use std::time::{Duration, Instant};

use voisu_core::{TELEMETRY_SCHEMA, millis_between, stop_anchored_timings};

#[test]
fn stop_anchored_fields_exclude_a_two_second_speech_interval() {
    let recording_start = Instant::now();
    let speech = Duration::from_millis(2_500);
    let utterance_end = recording_start + speech;
    let finalized_at = utterance_end + Duration::from_millis(40);
    let delivered_at = utterance_end + Duration::from_millis(80);

    let timings = stop_anchored_timings(recording_start, utterance_end, finalized_at, delivered_at);

    assert_eq!(TELEMETRY_SCHEMA, 2);
    assert_eq!(timings.recording_duration_ms, 2_500);
    assert_eq!(timings.stop_to_finalized_ms, 40);
    assert_eq!(timings.stop_to_delivered_ms, 80);

    let speech_ms = u64::try_from(speech.as_millis()).unwrap();
    assert!(
        timings.stop_to_finalized_ms < speech_ms,
        "stop_to_finalized_ms leaked speaking time: {}",
        timings.stop_to_finalized_ms
    );
    assert!(
        timings.stop_to_delivered_ms < speech_ms,
        "stop_to_delivered_ms leaked speaking time: {}",
        timings.stop_to_delivered_ms
    );

    // Start-anchored measurement of the same Instants *does* include speech.
    // If the helper ever switched anchors, the equalities above would break
    // and these inequalities would fail.
    let start_anchored_finalized = millis_between(recording_start, finalized_at);
    let start_anchored_delivered = millis_between(recording_start, delivered_at);
    assert_eq!(start_anchored_finalized, 2_540);
    assert_eq!(start_anchored_delivered, 2_580);
    assert_ne!(timings.stop_to_finalized_ms, start_anchored_finalized);
    assert_ne!(timings.stop_to_delivered_ms, start_anchored_delivered);
}
