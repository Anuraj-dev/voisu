//! L3 trusted catalog, crash-safe model store, and installer (R3–R5).
//!
//! Packages ship the versioned catalog and runtime, not weights. The catalog
//! retains a Pilot Candidate, but production selection and capture stay gated.

use std::time::Duration;

pub const INSTALL_DEADLINE: Duration = Duration::from_secs(30 * 60);
pub const NO_PROGRESS: Duration = Duration::from_secs(30);

mod catalog;
pub(crate) mod consented;
mod consented_catalog;
mod consented_pins;
mod consented_runtime;
mod development;
mod fetch;
mod hardware;
mod health;
mod installer;
mod receipt;
mod safe_fs;
mod store;
mod suggest;
mod url_policy;

pub(crate) fn consented_worker_files() -> &'static [(&'static str, &'static [u8])] {
    consented_catalog::WORKER_FILES
}
pub(crate) use receipt::FilePin;

pub use catalog::{
    Catalog, CatalogEntry, CatalogFile, DeviceSupport, FileKind, LicenseTerms, Provenance,
    Redistribution, RuntimeAbi, SHA256_HEX_LEN, ci_fixture_entry, development_selection,
    pilot_candidate, production_selection, sha256_hex, shipped_catalog, verify_file_digest,
};
pub(crate) use consented::consented_receipt_exists;
pub use consented::{
    ConsentedConsent, ConsentedError, ConsentedFilePin, ConsentedHealth, ConsentedInstallRequest,
    ConsentedLayout, ConsentedReceipt, ConsentedRuntimeEntry, VerifiedConsentedInstall,
    install_consented_install, remove_consented_install, verify_consented_install,
    verify_consented_install_full,
};
pub use consented_catalog::{
    CONSENT_TEXT, CONSENTED_ENTRY, ConsentedCatalogEntry, DownloadPin,
    ESTIMATED_EXTRACTED_RUNTIME_BYTES, EXPECTED_HEALTH_TEXT, REQUIRED_FREE_BYTES,
    consented_health_clip,
};
pub use consented_pins::TOTAL_DOWNLOAD_BYTES;
pub use consented_runtime::{
    ConsentedCommandRunner, ConsentedRuntimeBuilder, OfflineRuntimeBuilder, RuntimeBuild,
    SystemConsentedCommandRunner, audit_runtime_tree,
};
pub use development::{
    DEV_MODEL_IDS, DEVELOPMENT_MANIFEST_VERSION, DEVELOPMENT_ROOT_NAME, DevCatalogEntry,
    DevFilePin, DevLimits, DevManifest, DevManifestHandle, DevModelStatus, DevRuntimeKind,
    DevRuntimeManifest, DevelopmentError, DevelopmentStore, development_catalog,
    development_catalog_entry, development_receipt,
};
pub use fetch::{
    ArtifactFetcher, FetchError, FetchRequest, FetchResponse, FetchedSummary, ProductionHttps,
    ScriptedFetcher, ScriptedHop, follow_catalog_redirects,
};
pub use hardware::{
    CommandOutput, CommandRunner, GpuProbe, HostHardware, NvidiaGpu, RunError, SystemCommandRunner,
    parse_nvidia_smi, probe_hardware,
};
pub use health::{CandidateHealth, HealthError, HealthProbe, HealthReport, verify_candidate};
pub use installer::{
    InstallAbort, InstallClock, InstallConsent, InstallError, InstallIo, InstallRequest,
    MaintenanceError, MaintenanceKind, MaintenanceReservation, install_entry,
};
pub use receipt::{
    ActiveReceipt, ReceiptError, encode as encode_receipt, from_entry, parse as parse_receipt,
    retained_receipt_path, store_atomic as store_receipt_atomic,
};
pub use safe_fs::{SafeFsError, ensure_private_dir, relative_file_name};
pub use store::{
    ModelLease, ModelStore, StoreError, StoreLock, legacy_models_dir, models_dir, models_dir_from,
    models_dir_from_state, resolve_models_root,
};
pub use suggest::{
    CpuReason, DISTIL_MIN_TOTAL_VRAM_MIB, DISTIL_MODEL_ID, LocalSuggestion, PARAKEET_MODEL_ID,
    suggest_local, suggestion_lines, warnings,
};
pub use url_policy::{CatalogUrl, UrlPolicyError, validate_catalog_url};

use crate::local_worker::local_runtime_admission;

pub const LOCAL_PILOT_ENV: &str = "VOISU_ENABLE_LOCAL_PILOT";

/// Whether this daemon was explicitly launched as the developer Local pilot.
#[must_use]
pub fn development_local_enabled() -> bool {
    std::env::var(LOCAL_PILOT_ENV)
        .ok()
        .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "yes"))
}

/// Select the runtime model without changing the production catalog decision.
#[must_use]
pub fn runtime_selection(catalog: &Catalog) -> Option<&CatalogEntry> {
    if development_local_enabled() {
        development_selection(catalog)
    } else {
        production_selection(catalog)
    }
}

/// Runtime admission stays production-gated unless the explicitly host-only
/// developer Local pilot flag is present.
#[must_use]
pub fn observe_for_admission() -> bool {
    let _ = (INSTALL_DEADLINE, NO_PROGRESS);
    local_runtime_admission()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_retains_one_unmeasured_pilot_candidate() {
        let catalog = shipped_catalog();
        let candidate = pilot_candidate(&catalog).expect("Pilot Candidate");
        assert_eq!(candidate.id, "whisper-cpp-ggml-base.en");
        assert!(production_selection(&catalog).is_none());
        assert!(catalog.entries.len() >= 2);
        assert!(
            catalog
                .entries
                .iter()
                .all(|entry| !entry.id.to_ascii_lowercase().contains("ollama"))
        );
    }

    #[test]
    fn production_admission_stays_closed_before_measurement() {
        assert!(!observe_for_admission());
    }

    #[test]
    fn development_selection_is_scoped_to_the_pilot_candidate() {
        let catalog = shipped_catalog();
        assert_eq!(
            development_selection(&catalog).map(|entry| entry.id),
            Some("whisper-cpp-ggml-base.en")
        );
        assert!(production_selection(&catalog).is_none());
    }

    #[test]
    fn only_development_service_enables_the_local_pilot() {
        let dev = include_str!("../../../../packaging/voisu-dev.service");
        let production = include_str!("../../../../packaging/voisu.service");
        assert!(
            dev.lines()
                .any(|line| line == "Environment=VOISU_ENABLE_LOCAL_PILOT=1")
        );
        assert!(dev.lines().any(|line| line == "Conflicts=voisu.service"));
        assert!(!production.contains("VOISU_ENABLE_LOCAL_PILOT"));
    }
}
