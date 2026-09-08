//! R5 worker protocol, timing, and resource bounds.
//!
//! These numbers are locked engineering **targets**, not measurements. Filling
//! them with invented bakeoff results is a contract violation.

use std::time::Duration;

/// Signed 16-bit little-endian, mono, 16 kHz.
pub const PCM_SAMPLE_RATE_HZ: u32 = 16_000;
pub const PCM_CHANNELS: u16 = 1;
pub const PCM_BITS: u16 = 16;
/// 600 seconds × 16_000 Hz × 2 bytes.
pub const MAX_PCM_BYTES: usize = 19_200_000;
pub const MAX_RECORDING: Duration = Duration::from_secs(600);

/// 4-byte length prefix; at most 64 KiB JSON per control frame.
pub const MAX_JSON_FRAME_BYTES: usize = 64 * 1024;
pub const MAX_JSON_DEPTH: usize = 8;
pub const MAX_JSON_FIELDS: usize = 64;
pub const MAX_METADATA_BYTES: usize = 16 * 1024;

/// UTF-8 Transcript cap. Empty/silence is an explicit no-text outcome.
pub const MAX_TRANSCRIPT_BYTES: usize = 24 * 1024;

/// Native stderr drain cap; at most 4 KiB allowlisted structured error metadata.
pub const MAX_RETAINED_STDERR_BYTES: usize = 4 * 1024;
pub const MAX_ERROR_METADATA_BYTES: usize = 4 * 1024;
pub const MAX_CACHE_BYTES: u64 = 256 * 1024 * 1024;

/// Load includes verification and first inference. No capture until Ready.
pub const LOAD_DEADLINE: Duration = Duration::from_secs(60);

/// Absolute Stop processing budget across capture finalization, optional
/// recovery write, inference, deterministic formatting, and Delivery.
pub const STOP_PROCESSING: Duration = Duration::from_secs(45);

pub const CANCEL_GRACE: Duration = Duration::from_millis(500);
pub const REAP_OBSERVE: Duration = Duration::from_secs(2);
/// IPC margin on top of Local processing plus cleanup. Not the Cloud
/// `PROCESSING_RESPONSE_DEADLINE`.
pub const IPC_MARGIN: Duration = Duration::from_secs(3);

pub const MAX_RESTARTS: usize = 3;
pub const RESTART_WINDOW: Duration = Duration::from_secs(5 * 60);

pub const MAX_WORKER_TASKS: u32 = 64;
pub const MAX_CPU_CORES: u32 = 4;
pub const MAX_MEMORY_BYTES: u64 = 4 * 1024 * 1024 * 1024;
pub const MAX_VRAM_FRACTION_NUM: u64 = 75;
pub const MAX_VRAM_FRACTION_DEN: u64 = 100;

/// CLI/daemon Local response timeout: processing + cleanup + IPC margin.
#[must_use]
pub fn local_response_deadline() -> Duration {
    STOP_PROCESSING + CANCEL_GRACE + REAP_OBSERVE + IPC_MARGIN
}

/// At most 4 GiB and at most half physical RAM.
#[must_use]
pub fn worker_memory_ceiling_bytes(physical_ram: u64) -> u64 {
    MAX_MEMORY_BYTES.min(physical_ram / 2)
}

/// At most half host cores and at most 4, with a one-core floor.
#[must_use]
pub fn worker_cpu_quota_cores(host_cores: u32) -> u32 {
    let half = host_cores / 2;
    half.clamp(1, MAX_CPU_CORES)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system::PROCESSING_RESPONSE_DEADLINE;

    #[test]
    fn pcm_cap_matches_six_hundred_seconds_of_s16le_mono_16k() {
        assert_eq!(MAX_PCM_BYTES, 16_000 * 2 * 600);
        assert_eq!(MAX_RECORDING, Duration::from_secs(600));
    }

    #[test]
    fn local_response_deadline_is_not_the_cloud_budget() {
        let local = local_response_deadline();
        assert_eq!(local, Duration::from_millis(45_000 + 500 + 2_000 + 3_000));
        assert_ne!(local, PROCESSING_RESPONSE_DEADLINE);
    }

    #[test]
    fn memory_ceiling_never_exceeds_four_gib_or_half_ram() {
        assert_eq!(
            worker_memory_ceiling_bytes(8 * MAX_MEMORY_BYTES),
            MAX_MEMORY_BYTES
        );
        assert_eq!(
            worker_memory_ceiling_bytes(2 * 1024 * 1024 * 1024),
            1024 * 1024 * 1024
        );
    }

    #[test]
    fn cpu_quota_is_half_host_capped_at_four_with_one_core_floor() {
        assert_eq!(worker_cpu_quota_cores(1), 1);
        assert_eq!(worker_cpu_quota_cores(2), 1);
        assert_eq!(worker_cpu_quota_cores(8), 4);
        assert_eq!(worker_cpu_quota_cores(16), 4);
    }

    #[test]
    fn r5_table_is_locked_as_targets_not_measurements() {
        assert_eq!(PCM_SAMPLE_RATE_HZ, 16_000);
        assert_eq!(PCM_CHANNELS, 1);
        assert_eq!(PCM_BITS, 16);
        assert_eq!(MAX_JSON_FRAME_BYTES, 64 * 1024);
        assert_eq!(MAX_JSON_DEPTH, 8);
        assert_eq!(MAX_JSON_FIELDS, 64);
        assert_eq!(MAX_METADATA_BYTES, 16 * 1024);
        assert_eq!(MAX_TRANSCRIPT_BYTES, 24 * 1024);
        assert_eq!(MAX_RETAINED_STDERR_BYTES, 4 * 1024);
        assert_eq!(MAX_ERROR_METADATA_BYTES, 4 * 1024);
        assert_eq!(MAX_CACHE_BYTES, 256 * 1024 * 1024);
        assert_eq!(MAX_WORKER_TASKS, 64);
        assert_eq!(MAX_VRAM_FRACTION_NUM, 75);
        assert_eq!(MAX_VRAM_FRACTION_DEN, 100);
        assert_eq!(LOAD_DEADLINE, Duration::from_secs(60));
        assert_eq!(STOP_PROCESSING, Duration::from_secs(45));
        assert_eq!(CANCEL_GRACE, Duration::from_millis(500));
        assert_eq!(REAP_OBSERVE, Duration::from_secs(2));
        assert_eq!(MAX_RESTARTS, 3);
        assert_eq!(RESTART_WINDOW, Duration::from_secs(300));
    }
}
