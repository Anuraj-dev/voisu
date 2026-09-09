//! L6 R8 supported-profile packaging contracts.
//!
//! Repository tests prove packaging and rollback *contracts*. Real-host
//! install, microphone, Trigger Key uniqueness, suspend, and process-tree
//! network traces stay orchestrator-owned and are never claimed from CI.

mod ownership;
mod rollback;

use std::path::Path;

use crate::local_worker::{HostProfile, HostRole, locked_host_profiles};

pub use ownership::{
    USER_OWNED_HINT, UserOwnedTree, package_may_remove, user_owned_trees, user_tree_survives,
};
pub use rollback::{
    BinaryIdentity, BinaryRollbackError, ModelRepair, WorkerDeathAction, annotate_mode_message,
    binary_rollback, mode_after_worker_death, repair_from_cloud_switch, repair_from_setup,
    worker_death_action,
};

/// First supported product target. Omarchy/Arch Hyprland is a pilot only.
pub const FEDORA_KDE_WAYLAND: &str = "fedora-kde-wayland";
/// Pilot host. A pass here is not Fedora packaging proof.
pub const OMARCHY_ARCH_HYPRLAND: &str = "omarchy-arch-hyprland";

/// R8 packaging rows. Unit tests own the repository assertions; host rows
/// stay pending without orchestrator evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PackagingScenario {
    CleanInstall,
    OfflineRestart,
    UpgradePrivateSettings,
    BadModelRepair,
    RetainedModelRestore,
    SupportedBinaryRollback,
    WorkerDeath,
    UninstallOwnership,
}

/// Who can mark a scenario complete.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvidenceOwner {
    Repository,
    OrchestratorHost,
}

/// One R8 packaging row plus who must prove it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScenarioContract {
    pub scenario: PackagingScenario,
    pub owner: EvidenceOwner,
    /// Repository code never sets this. Host proof is orchestrator-owned.
    pub host_claimed: bool,
}

/// Documented host/product gates that CI must not auto-pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostGate {
    pub id: &'static str,
    pub summary: &'static str,
    pub profile: &'static str,
    pub owner: EvidenceOwner,
}

/// Named baseline quality failure. `waived` is always false.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BaselineDefect {
    pub id: &'static str,
    pub summary: &'static str,
    pub waived: bool,
    pub blocks_local_ship: bool,
}

/// Supported profiles in lock order: Fedora product, then Hyprland pilot.
#[must_use]
pub fn supported_profiles() -> [HostProfile; 2] {
    locked_host_profiles()
}

#[must_use]
pub fn first_supported_profile() -> HostProfile {
    supported_profiles()[0]
}

#[must_use]
pub fn packaging_scenarios() -> [ScenarioContract; 8] {
    [
        ScenarioContract {
            scenario: PackagingScenario::CleanInstall,
            owner: EvidenceOwner::Repository,
            host_claimed: false,
        },
        ScenarioContract {
            scenario: PackagingScenario::OfflineRestart,
            owner: EvidenceOwner::Repository,
            host_claimed: false,
        },
        ScenarioContract {
            scenario: PackagingScenario::UpgradePrivateSettings,
            owner: EvidenceOwner::Repository,
            host_claimed: false,
        },
        ScenarioContract {
            scenario: PackagingScenario::BadModelRepair,
            owner: EvidenceOwner::Repository,
            host_claimed: false,
        },
        ScenarioContract {
            scenario: PackagingScenario::RetainedModelRestore,
            owner: EvidenceOwner::Repository,
            host_claimed: false,
        },
        ScenarioContract {
            scenario: PackagingScenario::SupportedBinaryRollback,
            owner: EvidenceOwner::Repository,
            host_claimed: false,
        },
        ScenarioContract {
            scenario: PackagingScenario::WorkerDeath,
            owner: EvidenceOwner::Repository,
            host_claimed: false,
        },
        ScenarioContract {
            scenario: PackagingScenario::UninstallOwnership,
            owner: EvidenceOwner::Repository,
            host_claimed: false,
        },
    ]
}

/// Host-only gates. Missing evidence is a fail, never a pass.
#[must_use]
pub fn host_gates() -> &'static [HostGate] {
    &[
        HostGate {
            id: "fedora-kde-wayland-product",
            summary: "Fedora KDE Wayland packaged install on a clean account",
            profile: FEDORA_KDE_WAYLAND,
            owner: EvidenceOwner::OrchestratorHost,
        },
        HostGate {
            id: "omarchy-arch-hyprland-pilot",
            summary: "Omarchy/Arch Hyprland pilot; not Fedora product proof",
            profile: OMARCHY_ARCH_HYPRLAND,
            owner: EvidenceOwner::OrchestratorHost,
        },
        HostGate {
            id: "real-microphone",
            summary: "real microphone Recording on the claimed profile",
            profile: FEDORA_KDE_WAYLAND,
            owner: EvidenceOwner::OrchestratorHost,
        },
        HostGate {
            id: "trigger-key-uniqueness",
            summary: "exact Trigger Key uniqueness on the live desktop",
            profile: FEDORA_KDE_WAYLAND,
            owner: EvidenceOwner::OrchestratorHost,
        },
        HostGate {
            id: "suspend",
            summary: "suspend/resume leaves Local readiness truthful",
            profile: FEDORA_KDE_WAYLAND,
            owner: EvidenceOwner::OrchestratorHost,
        },
        HostGate {
            id: "process-tree-offline",
            summary: "process-tree network trace across Local startup/Start/Replay",
            profile: FEDORA_KDE_WAYLAND,
            owner: EvidenceOwner::OrchestratorHost,
        },
    ]
}

/// Open baseline failures. None are waived by L6.
#[must_use]
pub fn baseline_defects() -> &'static [BaselineDefect] {
    &[
        BaselineDefect {
            id: "rust-toolchain-toml",
            summary: "rust-toolchain.toml does not pin default CI jobs",
            waived: false,
            blocks_local_ship: true,
        },
        BaselineDefect {
            id: "trigger-key-uniqueness",
            summary: "portal Trigger Key uniqueness is not enforced in-process",
            waived: false,
            blocks_local_ship: true,
        },
        BaselineDefect {
            id: "locked-english-bakeoff",
            summary: "held-out English bakeoff has no winner and is not in CI",
            waived: false,
            blocks_local_ship: true,
        },
    ]
}

/// Repository CI commands L6 requires. Absence is a fail, not a waiver.
#[must_use]
pub fn required_ci_needles() -> &'static [&'static str] {
    &[
        "cargo test --workspace --locked",
        "cargo test --manifest-path tools/transcript-quality/Cargo.toml --locked",
        "cargo clippy --all-targets --workspace --locked -- -D warnings",
        "cargo clippy --workspace --all-targets --features overlay --locked -- -D warnings",
        "cargo fmt --all -- --check",
        "RUSTDOCFLAGS=\"-D warnings\" cargo doc --no-deps --locked",
        "dtolnay/rust-toolchain@1.92.0",
        "bash packaging/tests/local-asr-release-gate.sh",
    ]
}

/// A Hyprland pilot pass must not certify the Fedora product profile.
#[must_use]
pub fn pilot_proves_fedora(pilot: HostRole, product: HostRole) -> bool {
    matches!(
        (pilot, product),
        (
            HostRole::FirstSupportedProduct,
            HostRole::FirstSupportedProduct
        )
    )
}

/// Host gates stay unclaimed unless the orchestrator supplies evidence.
#[must_use]
pub fn host_gate_claimed(_gate: &HostGate) -> bool {
    false
}

/// Models belong under StateDirectory so ProtectSystem=strict can write them.
#[must_use]
pub fn models_live_under_state(models: &Path, state_root: &Path) -> bool {
    models.starts_with(state_root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_model::bakeoff_winner;
    use crate::local_worker::{GateVerdict, evaluate_report};
    use std::fs;
    use voisu_core::AsrMode;

    fn workspace_root() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .expect("crates/voisu-app lives two levels below the workspace")
            .to_path_buf()
    }

    #[test]
    fn fedora_kde_wayland_is_the_first_supported_product() {
        let profiles = supported_profiles();
        assert_eq!(profiles[0].id, FEDORA_KDE_WAYLAND);
        assert_eq!(profiles[0].role, HostRole::FirstSupportedProduct);
        assert_eq!(profiles[1].id, OMARCHY_ARCH_HYPRLAND);
        assert_eq!(profiles[1].role, HostRole::PilotNotProductProof);
        assert!(!pilot_proves_fedora(profiles[1].role, profiles[0].role));
    }

    #[test]
    fn packaging_rows_exist_and_are_not_host_claimed() {
        let rows = packaging_scenarios();
        assert_eq!(rows.len(), 8);
        assert!(rows.iter().all(|row| !row.host_claimed));
        assert!(
            rows.iter()
                .any(|row| row.scenario == PackagingScenario::CleanInstall)
        );
        assert!(
            rows.iter()
                .any(|row| row.scenario == PackagingScenario::UninstallOwnership)
        );
    }

    #[test]
    fn host_gates_stay_orchestrator_owned_and_unclaimed() {
        for gate in host_gates() {
            assert_eq!(gate.owner, EvidenceOwner::OrchestratorHost);
            assert!(!host_gate_claimed(gate));
        }
    }

    #[test]
    fn baseline_defects_are_named_and_not_waived() {
        let defects = baseline_defects();
        assert!(defects.iter().any(|d| d.id == "rust-toolchain-toml"));
        assert!(defects.iter().any(|d| d.id == "trigger-key-uniqueness"));
        assert!(defects.iter().any(|d| d.id == "locked-english-bakeoff"));
        assert!(defects.iter().all(|d| !d.waived && d.blocks_local_ship));
        // The rust-toolchain defect is named above and pinned via the
        // dtolnay needle in `repository_ci_keeps_locked_fmt_clippy_doc_msrv_and_quality`;
        // do not assert on rust-toolchain.toml absence here so fixing the
        // defect does not break this test.
        let report = evaluate_report(
            crate::local_worker::RuntimeFamily::WhisperCppProcess,
            &[],
            vec![],
            vec!["no locked corpus in CI".into()],
            None,
            false,
        );
        assert_eq!(report.verdict, GateVerdict::PendingEvidence);
        // The Arch pilot winner is elected for host testing, but the locked
        // English bakeoff defect still blocks any Local ship claim.
        let winner =
            bakeoff_winner(&crate::local_model::shipped_catalog()).expect("Arch pilot winner");
        assert_eq!(winner.id, "whisper-cpp-ggml-base.en");
        assert!(
            defects
                .iter()
                .any(|d| d.id == "locked-english-bakeoff" && d.blocks_local_ship && !d.waived)
        );
    }

    #[test]
    fn repository_ci_keeps_locked_fmt_clippy_doc_msrv_and_quality() {
        let ci = fs::read_to_string(workspace_root().join(".github/workflows/ci.yml"))
            .expect("CI workflow");
        for needle in required_ci_needles() {
            assert!(ci.contains(needle), "CI must contain {needle}");
        }
    }

    #[test]
    fn cloud_annotation_is_opt_out_not_repair() {
        let message = annotate_mode_message(AsrMode::Cloud, "ASR mode set to cloud".into());
        assert!(message.contains("opt-out"));
        assert!(message.contains("not a Local repair"));
        let local = annotate_mode_message(AsrMode::Local, "ASR mode set to local".into());
        assert_eq!(local, "ASR mode set to local");
        assert!(!repair_from_cloud_switch());
        assert!(repair_from_setup(true));
        assert!(!repair_from_setup(false));
    }

    #[test]
    fn worker_death_keeps_local_selection() {
        assert_eq!(
            worker_death_action(),
            WorkerDeathAction::StayUnavailable {
                switch_to_cloud: false
            }
        );
        assert_eq!(mode_after_worker_death(AsrMode::Local), AsrMode::Local);
    }

    #[test]
    fn pre_mode_binary_cannot_preserve_offline_guarantee() {
        let current = BinaryIdentity {
            version: "0.57.0",
            protocol: voisu_core::PROTOCOL_VERSION,
            capabilities: &[voisu_core::ASR_MODE_V1],
        };
        let compatible = BinaryIdentity {
            version: "0.56.0",
            protocol: voisu_core::PROTOCOL_VERSION,
            capabilities: &[voisu_core::ASR_MODE_V1],
        };
        let pre_mode = BinaryIdentity {
            version: "0.43.2",
            protocol: voisu_core::PROTOCOL_VERSION,
            capabilities: &[],
        };
        assert!(binary_rollback(&current, &compatible).is_ok());
        assert_eq!(
            binary_rollback(&current, &pre_mode),
            Err(BinaryRollbackError::PreModeRequiresMigration)
        );
    }

    #[test]
    fn packaged_unit_writes_models_through_state_directory() {
        let unit = include_str!("../../../../packaging/voisu.service");
        assert!(unit.contains("StateDirectory=voisu"));
        assert!(
            !unit.lines().any(|line| line.trim() == "ReadWritePaths=%h"),
            "fresh-home %h ReadWritePaths must not return"
        );
        let models = crate::local_model::models_dir_from_state(
            Some("/tmp/state".into()),
            Some("/home/raja".into()),
        )
        .unwrap();
        assert!(models_live_under_state(
            &models,
            std::path::Path::new("/tmp/state/voisu")
        ));
        assert_eq!(models, std::path::PathBuf::from("/tmp/state/voisu/models"));
    }

    #[test]
    fn repair_is_setup_with_retained_receipt_not_cloud() {
        assert!(
            ModelRepair::SetupRestore {
                verified_receipt: true
            }
            .is_repair()
        );
        assert!(!ModelRepair::CloudSwitch.is_repair());
    }
}
