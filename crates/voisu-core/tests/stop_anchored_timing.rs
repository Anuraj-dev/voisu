use std::time::{Duration, Instant};

use voisu_core::{TELEMETRY_SCHEMA, stop_anchored_timings};

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
    assert!(timings.stop_to_finalized_ms < timings.recording_duration_ms);
    assert!(timings.stop_to_delivered_ms < timings.recording_duration_ms);
    assert_ne!(timings.stop_to_finalized_ms, 2_540);
    assert_ne!(timings.stop_to_delivered_ms, 2_580);
}
