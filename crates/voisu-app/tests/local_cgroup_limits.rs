//! #266 worker cgroup + resource envelope: limits math, placement, cleanup.
//!
//! Pure tests pin exact topologies (including tiny/exhausted hosts). The join
//! test attempts a real placement and accepts exactly two outcomes: verified
//! membership, or a fail-closed error that keeps Local unavailable. Cleanup
//! tests prove scope removal works when empty and refuses live scopes.

use std::path::PathBuf;

use voisu_app::local_worker::{
    MAX_CACHE_BYTES, MAX_MEMORY_BYTES, MAX_WORKER_TASKS, cache_usage_within_budget,
    cgroup_controllers, child_cgroup_matches, cleanup_worker_cgroup, cpu_max_cgroup_value,
    cpu_quota_per_sec_usec, cpu_quota_percent, current_cgroup_path, desired_cgroup_spec,
    gate_local_on_sandbox, host_cpu_cores, host_physical_ram_bytes, join_worker_cgroup,
    process_cgroup_path, record_resource_evidence, resolve_live_worker_limits,
    resolve_worker_limits, verify_cgroup_membership, vram_budget_bytes, worker_cpu_quota_cores,
    worker_memory_ceiling_bytes, worker_scope_name,
};

const GIB: u64 = 1024 * 1024 * 1024;

#[test]
fn memory_cpu_tasks_cache_follow_the_locked_formula() {
    // min(4 GiB, half RAM) / min(4 cores, half capacity, 1-core floor) / 64
    // tasks / 256 MiB cache.
    let full = resolve_worker_limits(32 * GIB, 16);
    assert_eq!(full.memory_max_bytes, MAX_MEMORY_BYTES);
    assert_eq!(full.cpu_quota_cores, 4);
    assert_eq!(full.tasks_max, MAX_WORKER_TASKS);
    assert_eq!(full.cache_max_bytes, MAX_CACHE_BYTES);

    let half = resolve_worker_limits(2 * GIB, 2);
    assert_eq!(half.memory_max_bytes, GIB);
    assert_eq!(half.cpu_quota_cores, 1);

    let tiny = resolve_worker_limits(GIB, 1);
    assert_eq!(tiny.memory_max_bytes, GIB / 2);
    assert_eq!(tiny.cpu_quota_cores, 1);
    assert_eq!(tiny.tasks_max, 64);
    assert_eq!(tiny.cache_max_bytes, 256 * 1024 * 1024);

    // The envelope is exactly the ceiling/quota functions: no second formula.
    assert_eq!(full.memory_max_bytes, worker_memory_ceiling_bytes(32 * GIB));
    assert_eq!(full.cpu_quota_cores, worker_cpu_quota_cores(16));
}

#[test]
fn live_resolution_matches_explicit_topology() {
    let live = resolve_live_worker_limits().expect("this host reports RAM");
    assert_eq!(live.physical_ram_bytes, host_physical_ram_bytes());
    assert_eq!(live.host_cores, host_cpu_cores());
    assert!(live.host_cores >= 1);
    let explicit = resolve_worker_limits(
        live.physical_ram_bytes.expect("checked above"),
        live.host_cores,
    );
    assert_eq!(live.limits, explicit);
    assert!(live.limits.memory_max_bytes > 0);
    assert!((1..=4).contains(&live.limits.cpu_quota_cores));
}

#[test]
fn vram_grant_is_measured_and_cache_overuse_is_refused() {
    assert_eq!(vram_budget_bytes(4_000_000_000), 3_000_000_000);
    assert_eq!(vram_budget_bytes(0), 0);
    // Resource exhaustion shape: exactly at budget passes, one byte over fails.
    assert!(cache_usage_within_budget(MAX_CACHE_BYTES));
    assert!(!cache_usage_within_budget(MAX_CACHE_BYTES + 1));
    assert!(!cache_usage_within_budget(u64::MAX));
    // CPU-only hosts record no VRAM budget at all.
    let evidence = record_resource_evidence(Some(8 * GIB), 8, None).expect("known RAM");
    assert_eq!(evidence.vram_budget_bytes, None);
    assert_eq!(evidence.limits, resolve_worker_limits(8 * GIB, 8));
    // Unknown RAM records nothing instead of inventing a ceiling.
    assert!(record_resource_evidence(None, 8, Some(1_000)).is_none());
}

#[test]
fn cgroup_property_values_carry_resolved_limits() {
    assert_eq!(cpu_max_cgroup_value(4), "400000 100000");
    assert_eq!(cpu_max_cgroup_value(1), "100000 100000");
    assert_eq!(cpu_quota_per_sec_usec(4), 4_000_000);
    assert_eq!(cpu_quota_percent(4), "400%");
    assert_eq!(cpu_quota_percent(1), "100%");

    let spec = desired_cgroup_spec(8 * GIB, 8, 4242);
    assert_eq!(spec.scope_name, "voisu-worker-4242.scope");
    assert_eq!(spec.memory_max_bytes, MAX_MEMORY_BYTES);
    assert_eq!(spec.cpu_quota_cores, 4);
    assert_eq!(spec.tasks_max, MAX_WORKER_TASKS);
    assert_eq!(worker_scope_name(7), "voisu-worker-7.scope");
}

#[test]
fn current_host_cgroup_is_recorded_truthfully() {
    let raw = std::fs::read_to_string("/proc/self/cgroup").expect("/proc/self/cgroup");
    let has_v2 = raw.lines().any(|line| line.starts_with("0::"));
    assert_eq!(current_cgroup_path().is_some(), has_v2);
    let file_controllers = std::fs::read_to_string("/sys/fs/cgroup/cgroup.controllers")
        .map(|text| {
            text.split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    assert_eq!(cgroup_controllers(), file_controllers);
    // Self-membership is consistent between the two readers.
    let pid = std::process::id();
    assert_eq!(process_cgroup_path(pid), current_cgroup_path());
    assert!(!verify_cgroup_membership(
        "voisu-worker-definitely-absent.scope"
    ));
}

#[test]
fn worker_scope_join_is_verified_or_fail_closed() {
    let pid = std::process::id();
    let ram = host_physical_ram_bytes().unwrap_or(8 * GIB);
    let spec = desired_cgroup_spec(ram, host_cpu_cores(), pid);
    match join_worker_cgroup(&spec) {
        Ok(placement) => {
            // Verified membership plus descendant controller inheritance.
            assert!(placement.member);
            assert!(placement.scope_dir.ends_with(&spec.scope_name));
            assert!(verify_cgroup_membership(&spec.scope_name));
            for need in ["cpu", "memory", "pids"] {
                assert!(
                    placement.controllers.iter().any(|have| have == need),
                    "scope must inherit {need}"
                );
            }
            // A live (occupied) scope must never tear down: cleanup refuses.
            assert!(
                cleanup_worker_cgroup(&placement.scope_dir).is_err(),
                "cleanup must refuse a scope that still holds the worker"
            );
            eprintln!("worker scope placed at {}", placement.scope_dir.display());
        }
        Err(error) => {
            // Fail-closed: no delegation here, so Local stays unavailable and
            // the error names the cgroup/user-manager gap.
            let message = error.to_string();
            eprintln!("worker scope unavailable (fail closed): {message}");
            assert!(
                message.contains("cgroup") || message.contains("user"),
                "error must name the gap: {message}"
            );
            assert!(
                gate_local_on_sandbox().is_err(),
                "a host that cannot place the worker must not pass the gate"
            );
        }
    }
}

#[test]
fn descendant_scope_check_matches_exact_fragment() {
    // Positive control for the inheritance reader: our own PID matches our own
    // cgroup tail, and a nonsense fragment never matches.
    let pid = std::process::id();
    let path = process_cgroup_path(pid).expect("self cgroup visible");
    let tail = path.rsplit('/').next().filter(|tail| !tail.is_empty());
    if let Some(tail) = tail {
        assert!(child_cgroup_matches(pid, tail));
    }
    assert!(!child_cgroup_matches(pid, "voisu-worker-definitely-absent"));
    assert!(!child_cgroup_matches(u32::MAX, "voisu-worker"));
}

#[test]
fn cleanup_removes_empty_scope_and_surfaces_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let scope: PathBuf = dir.path().join("voisu-worker-1.scope");
    std::fs::create_dir(&scope).expect("fake empty scope");
    cleanup_worker_cgroup(&scope).expect("empty scope removes");
    assert!(!scope.exists());
    // Errors surface instead of pretending cleanup happened.
    assert!(cleanup_worker_cgroup(&scope).is_err());
    assert!(cleanup_worker_cgroup(&dir.path().join("voisu-worker-missing.scope")).is_err());
}
