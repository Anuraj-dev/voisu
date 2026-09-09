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
mod production;
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
    CANCEL_GRACE, IPC_MARGIN, LOAD_DEADLINE, MAX_CACHE_BYTES, MAX_CPU_CORES,
    MAX_ERROR_METADATA_BYTES, MAX_JSON_DEPTH, MAX_JSON_FIELDS, MAX_JSON_FRAME_BYTES,
    MAX_MEMORY_BYTES, MAX_METADATA_BYTES, MAX_PCM_BYTES, MAX_RECORDING, MAX_RESTARTS,
    MAX_RETAINED_STDERR_BYTES, MAX_TRANSCRIPT_BYTES, MAX_VRAM_FRACTION_DEN, MAX_VRAM_FRACTION_NUM,
    MAX_WORKER_TASKS, PCM_BITS, PCM_CHANNELS, PCM_SAMPLE_RATE_HZ, REAP_OBSERVE, RESTART_WINDOW,
    STOP_PROCESSING, local_response_deadline, worker_cpu_quota_cores, worker_memory_ceiling_bytes,
};
pub use lifecycle::{LocalLifecycle, production_local_admission};
pub use production::UnavailableWorker;
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
    CloudCapabilitySentinel, LauncherPolicy, PackagedUnitRestrictions, RestrictionProbe,
    ScrubbedEnvironment, landlock_allowlist, local_unavailable_if_restrictions_fail,
    packaged_unit_restrictions, scrub_worker_environment,
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
