//! Schema-2 stop-anchored telemetry, measured from injected [`Instant`]s.
//!
//! Bakeoff latency must exclude speaking time. `stop_to_finalized_ms` and
//! `stop_to_delivered_ms` are durations from `utterance_end`, never from
//! Recording start. [`release_to_text_ms`](crate::LifecycleEvidence::release_to_text_ms)
//! remains start-anchored and is not produced here.

use std::time::{Duration, Instant};

/// The schema-2 trio: speech duration plus the two stop-anchored latencies.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StopAnchoredTimings {
    pub recording_duration_ms: u64,
    pub stop_to_finalized_ms: u64,
    pub stop_to_delivered_ms: u64,
}

/// Milliseconds of a measured duration, saturating on overflow.
pub fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Milliseconds from `start` to `end`. Saturates to 0 if `end` precedes `start`.
pub fn millis_between(start: Instant, end: Instant) -> u64 {
    duration_millis(end.saturating_duration_since(start))
}

/// Stop-anchored telemetry for one completed Recording.
///
/// `recording_duration_ms` is `recording_start` → `utterance_end`. Both
/// `stop_to_*` fields are `utterance_end` → the later Instant, so a 2 s speech
/// interval cannot appear in them.
pub fn stop_anchored_timings(
    recording_start: Instant,
    utterance_end: Instant,
    finalized_at: Instant,
    delivered_at: Instant,
) -> StopAnchoredTimings {
    StopAnchoredTimings {
        recording_duration_ms: millis_between(recording_start, utterance_end),
        stop_to_finalized_ms: millis_between(utterance_end, finalized_at),
        stop_to_delivered_ms: millis_between(utterance_end, delivered_at),
    }
}
