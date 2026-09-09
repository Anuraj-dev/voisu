//! Consented Local model install/repair. Never auto-downloads. Never probes Cloud keys.

use voisu_core::AsrMode;

use crate::local_model::{
    ActiveReceipt, CandidateHealth, CatalogEntry, InstallClock, InstallConsent, InstallIo,
    InstallRequest, MaintenanceError, MaintenanceKind, MaintenanceReservation, ModelStore,
    ProductionHttps, ReceiptError, ci_fixture_entry, install_entry, models_dir, parse_receipt,
    retained_receipt_path, shipped_catalog, store_receipt_atomic,
};
use crate::setup::WizardIo;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalSetupOutcome {
    Installed,
    Restored,
    Skipped,
}

#[must_use]
pub fn is_local_mode() -> bool {
    let Ok(state) = voisu_core::state_dir() else {
        return false;
    };
    matches!(
        crate::asr_mode::load_mode(&crate::config::config_path(), &state),
        Ok((AsrMode::Local, _))
    )
}

/// Injected install/repair so tests never touch the network or Cloud keys.
pub trait LocalSetupActions {
    fn catalog_lines(&self) -> Vec<String>;
    fn active_identity(&self) -> Option<String>;
    fn retained_identity(&self) -> Option<String>;
    fn restore_retained(&mut self) -> Result<String, String>;
    fn install_fixture(&mut self, consent: InstallConsent) -> Result<String, String>;
}

pub struct ProductionLocalSetup;

impl LocalSetupActions for ProductionLocalSetup {
    fn catalog_lines(&self) -> Vec<String> {
        catalog_presentation()
    }

    fn active_identity(&self) -> Option<String> {
        load_store()
            .ok()?
            .load_active()
            .ok()
            .flatten()
            .map(|receipt| receipt.catalog_id)
    }

    fn retained_identity(&self) -> Option<String> {
        load_retained().ok().map(|receipt| receipt.catalog_id)
    }

    fn restore_retained(&mut self) -> Result<String, String> {
        restore_retained_receipt()
    }

    fn install_fixture(&mut self, consent: InstallConsent) -> Result<String, String> {
        let entry = ci_fixture_entry();
        if entry.production_weights {
            return Err("no bakeoff winner; production weights are not downloaded".into());
        }
        if consent.bytes != entry.total_bytes() || consent.license_spdx != entry.license.spdx {
            return Err("consent does not match the catalog fixture".into());
        }
        let mut store = load_store()?;
        install_entry(InstallRequest {
            store: &mut store,
            entry,
            fetcher: &ProductionHttps,
            health: &CandidateHealth::default(),
            maintenance: &SetupMaintenance,
            consent,
            io: InstallIo::default(),
            clock: InstallClock::default(),
        })
        .map(|receipt| receipt.catalog_id)
        .map_err(|error| format!("{error:?}"))
    }
}

struct SetupMaintenance;

impl MaintenanceReservation for SetupMaintenance {
    fn acquire(&self) -> Result<MaintenanceKind, MaintenanceError> {
        Ok(MaintenanceKind::Idle)
    }
}

pub fn run(io: &mut dyn WizardIo) -> Result<LocalSetupOutcome, String> {
    run_with(io, &mut ProductionLocalSetup)
}

pub fn run_with(
    io: &mut dyn WizardIo,
    actions: &mut dyn LocalSetupActions,
) -> Result<LocalSetupOutcome, String> {
    io.writeln("Voisu Local Setup — explicit model install and repair.");
    io.writeln(
        "Cloud credential-maintenance is `voisu auth verify` and is never run from Local setup.",
    );
    io.writeln("Downloads stay consented Setup-only. Nothing is auto-downloaded.");
    io.writeln("");
    io.writeln("Catalog (no bakeoff winner is selected):");
    for line in actions.catalog_lines() {
        io.writeln(&format!("  {line}"));
    }
    match actions.active_identity() {
        Some(id) => io.writeln(&format!("Active model: {id}")),
        None => io.writeln("Active model: none (Local selected; model unavailable)"),
    }
    match actions.retained_identity() {
        Some(id) => io.writeln(&format!("Retained model: {id}")),
        None => io.writeln("Retained model: none"),
    }
    if actions.retained_identity().is_some()
        && ask_yes_no(
            io,
            "Restore the retained verified model without network?",
            true,
        )
    {
        let id = actions.restore_retained()?;
        io.writeln(&format!("Restored retained model {id} without network."));
        return Ok(LocalSetupOutcome::Restored);
    }
    let fixture = ci_fixture_entry();
    io.writeln(&format!(
        "Fixture {} is {} bytes, license {}.",
        fixture.id,
        fixture.total_bytes(),
        fixture.license.spdx
    ));
    if !ask_yes_no(
        io,
        "Download and install the catalog fixture? Type yes to consent.",
        false,
    ) {
        io.writeln("No download. Setup did not install a model.");
        return Ok(LocalSetupOutcome::Skipped);
    }
    let consent = InstallConsent {
        bytes: fixture.total_bytes(),
        license_spdx: fixture.license.spdx.to_owned(),
    };
    let id = actions.install_fixture(consent)?;
    io.writeln(&format!("Installed {id} after explicit consent."));
    Ok(LocalSetupOutcome::Installed)
}

#[must_use]
pub fn catalog_presentation() -> Vec<String> {
    shipped_catalog().entries.iter().map(catalog_line).collect()
}

fn catalog_line(entry: &CatalogEntry) -> String {
    let kind = if entry.production_weights {
        "unelected production weights; not downloaded"
    } else {
        "installable with consent"
    };
    format!(
        "{}  {} bytes  {}  {kind}",
        entry.id,
        entry.total_bytes(),
        entry.license.spdx
    )
}

pub fn restore_retained_receipt() -> Result<String, String> {
    let store = load_store().map_err(|error| error.to_string())?;
    let receipt = load_retained()?;
    store_receipt_atomic(store.root(), &receipt).map_err(|error| format!("{error:?}"))?;
    Ok(receipt.catalog_id)
}

fn load_store() -> Result<ModelStore, String> {
    let dir = models_dir().map_err(|error| format!("{error:?}"))?;
    ModelStore::open(dir).map_err(|error| format!("{error:?}"))
}

fn load_retained() -> Result<ActiveReceipt, String> {
    let store = load_store()?;
    let path = retained_receipt_path(store.root());
    let text =
        std::fs::read_to_string(&path).map_err(|_| "no retained model receipt".to_owned())?;
    parse_receipt(&text).map_err(|error| match error {
        ReceiptError::Missing => "no retained model receipt".to_owned(),
        other => format!("{other:?}"),
    })
}

fn ask_yes_no(io: &mut dyn WizardIo, question: &str, default_yes: bool) -> bool {
    let suffix = if default_yes { " [Y/n]" } else { " [y/N]" };
    loop {
        match io.prompt_line(&format!("{question}{suffix} ")) {
            None => return default_yes,
            Some(answer) => match answer.trim().to_ascii_lowercase().as_str() {
                "" => return default_yes,
                "y" | "yes" => return true,
                "n" | "no" => return false,
                _ => io.writeln("Please answer y or n."),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_doctor::CLOUD_CREDENTIAL_MAINTENANCE;
    use crate::local_model::{bakeoff_winner, from_entry};

    struct FakeIo {
        lines: Vec<Option<String>>,
        out: Vec<String>,
    }

    impl FakeIo {
        fn new(lines: Vec<&str>) -> Self {
            Self {
                lines: lines
                    .into_iter()
                    .map(|line| Some(line.to_owned()))
                    .collect(),
                out: Vec::new(),
            }
        }

        fn transcript(&self) -> String {
            self.out.join("\n")
        }
    }

    impl WizardIo for FakeIo {
        fn writeln(&mut self, line: &str) {
            self.out.push(line.to_owned());
        }

        fn prompt_line(&mut self, prompt: &str) -> Option<String> {
            self.out.push(prompt.to_owned());
            if self.lines.is_empty() {
                None
            } else {
                self.lines.remove(0)
            }
        }

        fn prompt_secret(&mut self, _prompt: &str) -> Option<String> {
            panic!("Local setup must not prompt for Cloud keys");
        }
    }

    struct ScriptedActions {
        catalog: Vec<String>,
        active: Option<String>,
        retained: Option<String>,
        restore: Result<String, String>,
        install: Result<String, String>,
        installed_consent: Option<InstallConsent>,
        restored: bool,
    }

    impl LocalSetupActions for ScriptedActions {
        fn catalog_lines(&self) -> Vec<String> {
            self.catalog.clone()
        }

        fn active_identity(&self) -> Option<String> {
            self.active.clone()
        }

        fn retained_identity(&self) -> Option<String> {
            self.retained.clone()
        }

        fn restore_retained(&mut self) -> Result<String, String> {
            self.restored = true;
            self.restore.clone()
        }

        fn install_fixture(&mut self, consent: InstallConsent) -> Result<String, String> {
            self.installed_consent = Some(consent);
            self.install.clone()
        }
    }

    #[test]
    fn refusing_consent_does_not_download() {
        let mut io = FakeIo::new(vec!["n"]);
        let mut actions = ScriptedActions {
            catalog: catalog_presentation(),
            active: None,
            retained: None,
            restore: Err("unused".into()),
            install: Ok("should-not-run".into()),
            installed_consent: None,
            restored: false,
        };
        let outcome = run_with(&mut io, &mut actions).unwrap();
        assert_eq!(outcome, LocalSetupOutcome::Skipped);
        assert!(actions.installed_consent.is_none());
        assert!(!actions.restored);
        let transcript = io.transcript();
        assert!(transcript.contains("never run from Local setup"));
        assert!(transcript.contains("Nothing is auto-downloaded"));
        assert!(transcript.contains("no bakeoff winner"));
        assert!(transcript.contains("unelected production weights"));
        assert!(!transcript.to_ascii_lowercase().contains("ollama"));
    }

    #[test]
    fn restore_is_preferred_and_uses_no_network() {
        let mut io = FakeIo::new(vec!["y"]);
        let mut actions = ScriptedActions {
            catalog: vec!["fixture".into()],
            active: Some("broken".into()),
            retained: Some("l3-health-fixture".into()),
            restore: Ok("l3-health-fixture".into()),
            install: Ok("should-not-run".into()),
            installed_consent: None,
            restored: false,
        };
        let outcome = run_with(&mut io, &mut actions).unwrap();
        assert_eq!(outcome, LocalSetupOutcome::Restored);
        assert!(actions.restored);
        assert!(actions.installed_consent.is_none());
        assert!(io.transcript().contains("without network"));
    }

    #[test]
    fn consent_is_required_before_install() {
        let mut io = FakeIo::new(vec!["yes"]);
        let mut actions = ScriptedActions {
            catalog: vec!["fixture".into()],
            active: None,
            retained: None,
            restore: Err("unused".into()),
            install: Ok("l3-health-fixture".into()),
            installed_consent: None,
            restored: false,
        };
        let outcome = run_with(&mut io, &mut actions).unwrap();
        assert_eq!(outcome, LocalSetupOutcome::Installed);
        let consent = actions.installed_consent.unwrap();
        let fixture = ci_fixture_entry();
        assert_eq!(consent.bytes, fixture.total_bytes());
        assert_eq!(consent.license_spdx, fixture.license.spdx);
    }

    #[test]
    fn restore_retained_receipt_rewrites_active_without_fetch() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = StoreHomeGuard::set(temp.path());
        let store = ModelStore::open(models_dir().unwrap()).unwrap();
        let fixture = ci_fixture_entry();
        let previous = from_entry(fixture, "old");
        let retained = from_entry(fixture, "kept");
        store_receipt_atomic(store.root(), &previous).unwrap();
        std::fs::write(
            retained_receipt_path(store.root()),
            crate::local_model::encode_receipt(&retained),
        )
        .unwrap();
        let id = restore_retained_receipt().unwrap();
        assert_eq!(id, "l3-health-fixture");
        assert_eq!(
            store.load_active().unwrap().unwrap().artifact_id,
            retained.artifact_id
        );
    }

    #[test]
    fn production_adapter_rejects_mismatched_consent_without_downloading() {
        let mut actions = ProductionLocalSetup;
        let error = actions
            .install_fixture(InstallConsent {
                bytes: 1,
                license_spdx: "MIT".into(),
            })
            .unwrap_err();
        assert!(error.contains("consent does not match"), "{error}");
        assert!(CLOUD_CREDENTIAL_MAINTENANCE.contains("Cloud credential-maintenance"));
        assert!(bakeoff_winner(&shipped_catalog()).is_none());
    }

    struct StoreHomeGuard {
        previous_data: Option<std::ffi::OsString>,
        previous_home: Option<std::ffi::OsString>,
    }

    impl StoreHomeGuard {
        fn set(path: &std::path::Path) -> Self {
            let previous_data = std::env::var_os("XDG_DATA_HOME");
            let previous_home = std::env::var_os("HOME");
            unsafe {
                std::env::set_var("XDG_DATA_HOME", path);
                std::env::set_var("HOME", path);
            }
            Self {
                previous_data,
                previous_home,
            }
        }
    }

    impl Drop for StoreHomeGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.previous_data {
                    Some(value) => std::env::set_var("XDG_DATA_HOME", value),
                    None => std::env::remove_var("XDG_DATA_HOME"),
                }
                match &self.previous_home {
                    Some(value) => std::env::set_var("HOME", value),
                    None => std::env::remove_var("HOME"),
                }
            }
        }
    }
}
