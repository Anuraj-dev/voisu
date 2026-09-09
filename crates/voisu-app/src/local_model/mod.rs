//! L3 trusted catalog, crash-safe model store, and installer (R3–R5).
//!
//! Packages ship the versioned catalog and runtime, not weights. Local ASR
//! capture stays gated until L4. No bakeoff winner is selected. Ollama is not
//! a candidate.

use std::time::Duration;

pub const INSTALL_DEADLINE: Duration = Duration::from_secs(30 * 60);
pub const NO_PROGRESS: Duration = Duration::from_secs(30);

mod catalog;
mod fetch;
mod health;
mod installer;
mod receipt;
mod safe_fs;
mod store;
mod url_policy;

pub use catalog::{
    Catalog, CatalogEntry, CatalogFile, DeviceSupport, FileKind, LicenseTerms, Provenance,
    Redistribution, RuntimeAbi, SHA256_HEX_LEN, bakeoff_winner, ci_fixture_entry, sha256_hex,
    shipped_catalog, verify_file_digest,
};
pub use fetch::{
    ArtifactFetcher, FetchError, FetchRequest, FetchResponse, ProductionHttps, ScriptedFetcher,
    ScriptedHop, follow_catalog_redirects,
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
pub use url_policy::{CatalogUrl, UrlPolicyError, validate_catalog_url};

use crate::local_worker::production_local_admission;

/// Admission observes the store and worker gate without enabling capture.
#[must_use]
pub fn observe_for_admission() -> bool {
    let _ = models_dir();
    let _ = (INSTALL_DEADLINE, NO_PROGRESS);
    production_local_admission()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l3_does_not_select_a_bakeoff_winner() {
        let catalog = shipped_catalog();
        assert!(bakeoff_winner(&catalog).is_none());
        assert!(catalog.entries.len() >= 2);
        assert!(
            catalog
                .entries
                .iter()
                .all(|entry| !entry.id.to_ascii_lowercase().contains("ollama"))
        );
    }

    #[test]
    fn production_admission_stays_gated() {
        assert!(!observe_for_admission());
    }
}
