//! Rollback: Cloud opt-out, Setup repair/restore, compatible binary only.

use voisu_core::{ASR_MODE_V1, AsrMode, PROTOCOL_VERSION};

/// Switching to Cloud opts out of later Local Recordings. It does not repair.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelRepair {
    CloudSwitch,
    SetupRestore { verified_receipt: bool },
}

impl ModelRepair {
    #[must_use]
    pub fn is_repair(self) -> bool {
        matches!(
            self,
            Self::SetupRestore {
                verified_receipt: true
            }
        )
    }
}

/// Worker death leaves Local selected and unavailable. It never selects Cloud.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerDeathAction {
    StayUnavailable { switch_to_cloud: bool },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BinaryIdentity<'a> {
    pub version: &'a str,
    pub protocol: u32,
    pub capabilities: &'a [&'a str],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BinaryRollbackError {
    ProtocolMismatch,
    PreModeRequiresMigration,
}

impl BinaryIdentity<'_> {
    #[must_use]
    pub fn mode_aware(&self) -> bool {
        self.protocol == PROTOCOL_VERSION && self.capabilities.contains(&ASR_MODE_V1)
    }
}

/// Cloud CLI/daemon text must say opt-out, not repair.
#[must_use]
pub fn annotate_mode_message(mode: AsrMode, message: String) -> String {
    match mode {
        AsrMode::Cloud => {
            format!(
                "{message}; Cloud is a user opt-out for subsequent Recordings, not a Local repair"
            )
        }
        AsrMode::Local => message,
    }
}

#[must_use]
pub fn repair_from_cloud_switch() -> bool {
    ModelRepair::CloudSwitch.is_repair()
}

#[must_use]
pub fn repair_from_setup(verified_receipt: bool) -> bool {
    ModelRepair::SetupRestore { verified_receipt }.is_repair()
}

#[must_use]
pub fn worker_death_action() -> WorkerDeathAction {
    WorkerDeathAction::StayUnavailable {
        switch_to_cloud: false,
    }
}

#[must_use]
pub fn mode_after_worker_death(selected: AsrMode) -> AsrMode {
    let WorkerDeathAction::StayUnavailable { switch_to_cloud } = worker_death_action();
    if switch_to_cloud {
        AsrMode::Cloud
    } else {
        selected
    }
}

/// Binary rollback is allowed only onto a mode-aware compatible version.
pub fn binary_rollback(
    current: &BinaryIdentity<'_>,
    target: &BinaryIdentity<'_>,
) -> Result<(), BinaryRollbackError> {
    let _ = current;
    if target.protocol != PROTOCOL_VERSION {
        return Err(BinaryRollbackError::ProtocolMismatch);
    }
    if !target.mode_aware() {
        return Err(BinaryRollbackError::PreModeRequiresMigration);
    }
    Ok(())
}
