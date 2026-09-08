//! Schema-2 stop-anchored telemetry from injected [`Instant`]s.

use std::time::{Duration, Instant};

/// The schema-2 trio: speech duration plus the two stop-anchored latencies.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StopAnchoredTimings {
    pub recording_duration_ms: u64,
    pub stop_to_finalized_ms: u64,
    pub stop_to_delivered_ms: u64,
}

pub(crate) fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn millis_between(start: Instant, end: Instant) -> u64 {
    duration_millis(end.saturating_duration_since(start))
}

/// `stop_to_*` are `utterance_end` → later, never Recording start → later.
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
