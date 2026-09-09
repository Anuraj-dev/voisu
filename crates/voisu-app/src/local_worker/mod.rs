//! Local ASR worker seams: L2 bakeoff harness plus L3 supervised lifecycle.
//!
//! This module is **not** product Local ASR. The daemon does not admit a Local
//! Recording path here; Local remains unavailable until L4. No model winner is
//! selected. Production weights are never downloaded.
//!
//! Terms follow `CONTEXT.md`: Recording, Transcript, Trigger Key, Delivery,
//! Overlay.

mod bakeoff;
mod bounds;
mod lifecycle;
mod protocol;
mod runtime;
mod sandbox;
mod seams;
mod supervisor;
mod whisper_cli;

pub use bakeoff::{
    BakeoffCase, BakeoffReport, COLD_READY_MAX_MS, CORPUS_CONTRACT_ID, CORPUS_VERSION, CaseKind,
    CaseOutcome, CriticalKind, DurationBand, FeasibilityRunner, GateVerdict, HostProfile, HostRole,
    SoakEvidence, Split, evaluate_report, locked_host_profiles, locked_thresholds,
    percentile_nearest_rank,
};
pub use bounds::{
    CANCEL_GRACE, IPC_MARGIN, LOAD_DEADLINE, LiveHostLimits, MAX_CACHE_BYTES, MAX_CPU_CORES,
    MAX_ERROR_METADATA_BYTES, MAX_JSON_DEPTH, MAX_JSON_FIELDS, MAX_JSON_FRAME_BYTES,
    MAX_MEMORY_BYTES, MAX_METADATA_BYTES, MAX_PCM_BYTES, MAX_RECORDING, MAX_RESTARTS,
    MAX_RETAINED_STDERR_BYTES, MAX_TRANSCRIPT_BYTES, MAX_VRAM_FRACTION_DEN, MAX_VRAM_FRACTION_NUM,
    MAX_WORKER_TASKS, PCM_BITS, PCM_CHANNELS, PCM_SAMPLE_RATE_HZ, REAP_OBSERVE, RESTART_WINDOW,
    ResourceEvidence, STOP_PROCESSING, WorkerCgroupSpec, WorkerResourceLimits,
    cache_usage_within_budget, cpu_max_cgroup_value, cpu_quota_per_sec_usec, cpu_quota_percent,
    desired_cgroup_spec, host_cpu_cores, host_physical_ram_bytes, local_response_deadline,
    record_resource_evidence, resolve_live_worker_limits, resolve_worker_limits, vram_budget_bytes,
    worker_cpu_quota_cores, worker_memory_ceiling_bytes,
};
pub use lifecycle::{LocalLifecycle, production_local_admission};
pub use protocol::{
    ControlFrame, Correlation, FrameError, PROTOCOL_VERSION, ProtocolVersion, WorkerFrame,
    decode_json_frame, encode_json_frame, is_silence_pcm, parse_control, parse_worker,
    validate_pcm, validate_transcript,
};
pub use runtime::{
    Eligibility, RuntimeCandidate, RuntimeError, RuntimeFamily, evaluation_order,
    refuse_production_weight_download, reject_forbidden_program, shipped_unit_candidates,
};
pub use sandbox::{
    AppliedEnvEvidence, CgroupEvidence, CloudCapabilitySentinel, CoreDumpEvidence, DeniedSyscall,
    FdScrubEvidence, HostSandboxEvidence, LandlockEvidence, LandlockRequest, LauncherPolicy,
    NoNewPrivsEvidence, PRODUCT_TARGET, PackagedUnitRestrictions, PreExecEvidence,
    RestrictionProbe, SandboxCapabilities, SandboxError, SandboxReady, ScrubbedEnvironment,
    SeccompEvidence, SyscallArch, VerifiedModel, apply_landlock_restrictions, apply_no_new_privs,
    apply_worker_environment, build_seccomp_filter, cgroup_controllers, child_cgroup_matches,
    cleanup_worker_cgroup, close_unrelated_fds, collect_host_evidence, core_rlimit_zero,
    current_cgroup_path, current_host_id, default_runtime_lib_dirs, denied_syscalls,
    detect_landlock_abi, disable_core_dumps, forbidden_env_reason, gate_local_on_sandbox,
    install_syscall_restrictions, is_dumpable_disabled, is_no_new_privs, join_worker_cgroup,
    landlock_allowlist, landlock_request_clean, local_unavailable_if_restrictions_fail,
    native_syscall_arch, packaged_unit_restrictions, pre_exec_lockdown, probe_sandbox_capabilities,
    process_cgroup_path, render_host_evidence_markdown, scope_controllers,
    scrub_worker_environment, verify_cgroup_membership, verify_model_file, worker_scope_name,
};
pub use seams::{
    CaptureSeam, DeliverySeam, FakeCapture, FakeDelivery, FinalizedRecording, SeamError,
    transcribe_through_supervisor,
};
pub use supervisor::{
    FakeWorker, ReapOutcome, RestartBudget, SupervisorError, TranscribeRequest, WorkerChild,
    WorkerOutcome, WorkerState, WorkerSupervisor, control_frame_from_json, spawn_program_allowed,
    worker_frame_from_json,
};
pub use whisper_cli::{PILOT_WHISPER_CLI, WhisperCppWorker};

#[cfg(test)]
mod product_unavailable_tests {
    #[test]
    fn product_daemon_does_not_grow_a_local_god_file() {
        let daemon = include_str!("../bin/voisu-daemon.rs");
        assert!(
            !daemon.contains("local_worker"),
            "L3 keeps worker lifecycle out of voisu-daemon.rs"
        );
        assert!(
            !daemon.contains("local_model"),
            "L3 keeps catalog/installer out of voisu-daemon.rs"
        );
        assert!(
            daemon.contains("CaptureKind::Start"),
            "Start still goes through the L1 admission seam"
        );
        assert!(
            daemon.contains("local_routing"),
            "L4 Local routing is a daemon call site"
        );
    }

    #[test]
    fn product_cli_mode_exists_without_installer_dump() {
        let cli = include_str!("../bin/voisu.rs");
        assert!(
            !cli.contains("local_worker"),
            "L3 keeps worker lifecycle out of voisu.rs"
        );
        assert!(
            !cli.contains("local_model"),
            "L3 keeps catalog/installer out of voisu.rs"
        );
        assert!(
            cli.contains("mode <local|cloud>"),
            "L1 mode command remains the CLI call site"
        );
    }
}
