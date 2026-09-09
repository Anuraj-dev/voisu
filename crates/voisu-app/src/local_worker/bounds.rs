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

/// Unified worker resource envelope for #266: every number derives from the
/// locked caps above, never from a measurement. The launcher copies these into
/// the user-manager-owned worker cgroup before model load; a worker that
/// cannot be placed under them fails Local closed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerResourceLimits {
    /// `min(4 GiB, half physical RAM)`.
    pub memory_max_bytes: u64,
    /// `min(4 cores, half host capacity)` with a one-core floor.
    pub cpu_quota_cores: u32,
    /// At most 64 tasks.
    pub tasks_max: u32,
    /// Private worker cache is at most 256 MiB.
    pub cache_max_bytes: u64,
}

/// Resolve the full envelope from host topology. Pure: callers pass measured
/// host values (see [`host_physical_ram_bytes`]/[`host_cpu_cores`]) so tests
/// can pin tiny hosts and prove floor behavior.
#[must_use]
pub fn resolve_worker_limits(physical_ram_bytes: u64, host_cores: u32) -> WorkerResourceLimits {
    WorkerResourceLimits {
        memory_max_bytes: worker_memory_ceiling_bytes(physical_ram_bytes),
        cpu_quota_cores: worker_cpu_quota_cores(host_cores),
        tasks_max: MAX_WORKER_TASKS,
        cache_max_bytes: MAX_CACHE_BYTES,
    }
}

/// Physical RAM in bytes from `/proc/meminfo`, or `None` where the host does
/// not expose it. `None` is fail-closed input: the launcher must refuse to
/// invent a ceiling.
#[must_use]
pub fn host_physical_ram_bytes() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kibibytes: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return kibibytes.checked_mul(1024);
        }
    }
    None
}

/// Online host cores, with a one-core floor when the platform cannot report.
#[must_use]
pub fn host_cpu_cores() -> u32 {
    std::thread::available_parallelism()
        .map(|cores| cores.get().min(u32::MAX as usize) as u32)
        .unwrap_or(1)
        .max(1)
}

/// Envelope for the live host: resolved limits plus the topology inputs they
/// came from, for feasibility evidence. `physical_ram_bytes` stays `None`
/// instead of guessing when `/proc/meminfo` is unavailable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LiveHostLimits {
    pub physical_ram_bytes: Option<u64>,
    pub host_cores: u32,
    pub limits: WorkerResourceLimits,
}

/// Resolve against the live host. Fails closed (`None`) when RAM is unknown:
/// without RAM there is no honest memory ceiling.
#[must_use]
pub fn resolve_live_worker_limits() -> Option<LiveHostLimits> {
    let physical_ram_bytes = host_physical_ram_bytes()?;
    let host_cores = host_cpu_cores();
    Some(LiveHostLimits {
        physical_ram_bytes: Some(physical_ram_bytes),
        host_cores,
        limits: resolve_worker_limits(physical_ram_bytes, host_cores),
    })
}

/// Measured GPU memory bound where applicable: at most 75% of device VRAM.
/// `None` total VRAM (CPU-only host) means no GPU budget is granted.
#[must_use]
pub fn vram_budget_bytes(total_vram_bytes: u64) -> u64 {
    total_vram_bytes.saturating_mul(MAX_VRAM_FRACTION_NUM) / MAX_VRAM_FRACTION_DEN
}

/// Private-cache budget check: the bounded worker cache must stay within
/// [`MAX_CACHE_BYTES`]. The launcher probes real usage before model load and
/// refuses an over-budget cache instead of silently growing it.
#[must_use]
pub fn cache_usage_within_budget(used_bytes: u64) -> bool {
    used_bytes <= MAX_CACHE_BYTES
}

/// `cpu.max` quota for a cgroup v2 scope: whole cores as microseconds of CPU
/// time per 100 ms period, per cpu.max `$MAX $PERIOD` syntax.
#[must_use]
pub fn cpu_max_cgroup_value(quota_cores: u32) -> String {
    format!("{} 100000", u64::from(quota_cores).saturating_mul(100_000))
}

/// `CPUQuotaPerSecUSec=` value for a transient-unit property map: one core is
/// one second of CPU time per second.
#[must_use]
pub fn cpu_quota_per_sec_usec(quota_cores: u32) -> u64 {
    u64::from(quota_cores).saturating_mul(1_000_000)
}

/// `CPUQuota=` percentage string for a transient-unit property map.
#[must_use]
pub fn cpu_quota_percent(quota_cores: u32) -> String {
    format!("{}%", u64::from(quota_cores).saturating_mul(100))
}

/// Worker cgroup shape: the user-manager-owned scope the launcher creates
/// before model load, carrying the resolved envelope. Enforcement itself lives
/// in `sandbox::join_worker_cgroup`; this struct is the shared contract so
/// limits math and cgroup wiring cannot drift apart.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerCgroupSpec {
    /// Leaf scope name, e.g. `voisu-worker-1234.scope`.
    pub scope_name: String,
    pub memory_max_bytes: u64,
    pub cpu_quota_cores: u32,
    pub tasks_max: u32,
}

/// Build the scope shape for one worker process. Wires the resolved envelope
/// (never raw host values) into the scope contract.
#[must_use]
pub fn desired_cgroup_spec(
    physical_ram_bytes: u64,
    host_cores: u32,
    worker_pid: u32,
) -> WorkerCgroupSpec {
    let limits = resolve_worker_limits(physical_ram_bytes, host_cores);
    WorkerCgroupSpec {
        scope_name: format!("voisu-worker-{worker_pid}.scope"),
        memory_max_bytes: limits.memory_max_bytes,
        cpu_quota_cores: limits.cpu_quota_cores,
        tasks_max: limits.tasks_max,
    }
}

/// Recorded resource evidence for one feasibility host: topology inputs,
/// resolved envelope, and the measured GPU bound where applicable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResourceEvidence {
    pub physical_ram_bytes: Option<u64>,
    pub host_cores: u32,
    pub limits: WorkerResourceLimits,
    /// `Some` only on a host with measured device VRAM; CPU-only hosts record
    /// `None` and grant no GPU budget.
    pub vram_budget_bytes: Option<u64>,
}

/// Record evidence from explicit inputs. No host probing inside, so tests pin
/// exact topologies and the launcher records exactly what it enforced.
#[must_use]
pub fn record_resource_evidence(
    physical_ram_bytes: Option<u64>,
    host_cores: u32,
    total_vram_bytes: Option<u64>,
) -> Option<ResourceEvidence> {
    let ram = physical_ram_bytes?;
    Some(ResourceEvidence {
        physical_ram_bytes: Some(ram),
        host_cores,
        limits: resolve_worker_limits(ram, host_cores),
        vram_budget_bytes: total_vram_bytes.map(vram_budget_bytes),
    })
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
    fn resolved_envelope_combines_all_locked_caps() {
        let gib = 1024 * 1024 * 1024_u64;
        let limits = resolve_worker_limits(32 * gib, 16);
        assert_eq!(limits.memory_max_bytes, MAX_MEMORY_BYTES);
        assert_eq!(limits.cpu_quota_cores, 4);
        assert_eq!(limits.tasks_max, MAX_WORKER_TASKS);
        assert_eq!(limits.cache_max_bytes, MAX_CACHE_BYTES);
    }

    #[test]
    fn resolved_envelope_floors_on_a_tiny_host() {
        let gib = 1024 * 1024 * 1024_u64;
        let limits = resolve_worker_limits(gib, 1);
        // Half of 1 GiB RAM, one-core floor, tasks/cache unchanged.
        assert_eq!(limits.memory_max_bytes, gib / 2);
        assert_eq!(limits.cpu_quota_cores, 1);
        assert_eq!(limits.tasks_max, 64);
        assert_eq!(limits.cache_max_bytes, 256 * 1024 * 1024);
    }

    #[test]
    fn live_resolution_refuses_unknown_ram() {
        assert!(record_resource_evidence(None, 8, None).is_none());
        assert!(resolve_live_worker_limits().is_some());
    }

    #[test]
    fn vram_budget_is_three_quarters_and_cpu_only_grants_nothing() {
        assert_eq!(vram_budget_bytes(8_000_000_000), 6_000_000_000);
        let evidence = record_resource_evidence(Some(8 * 1024 * 1024 * 1024), 8, None)
            .expect("known RAM records evidence");
        assert_eq!(evidence.vram_budget_bytes, None);
        let with_gpu = record_resource_evidence(Some(8 * 1024 * 1024 * 1024), 8, Some(1_000))
            .expect("known RAM records evidence");
        assert_eq!(with_gpu.vram_budget_bytes, Some(750));
    }

    #[test]
    fn cache_budget_rejects_overuse() {
        assert!(cache_usage_within_budget(MAX_CACHE_BYTES));
        assert!(!cache_usage_within_budget(MAX_CACHE_BYTES + 1));
    }

    #[test]
    fn cgroup_values_carry_whole_cores() {
        assert_eq!(cpu_max_cgroup_value(4), "400000 100000");
        assert_eq!(cpu_quota_per_sec_usec(4), 4_000_000);
        assert_eq!(cpu_quota_percent(1), "100%");
        assert_eq!(cpu_quota_percent(4), "400%");
    }

    #[test]
    fn scope_spec_names_the_worker_and_carries_resolved_limits() {
        let gib = 1024 * 1024 * 1024_u64;
        let spec = desired_cgroup_spec(2 * gib, 2, 4242);
        assert_eq!(spec.scope_name, "voisu-worker-4242.scope");
        assert_eq!(spec.memory_max_bytes, gib);
        assert_eq!(spec.cpu_quota_cores, 1);
        assert_eq!(spec.tasks_max, MAX_WORKER_TASKS);
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
