//! Consented Local model install/repair. Never auto-downloads. Never probes Cloud keys.

use std::sync::Mutex;

use voisu_core::{AsrMode, DaemonState, LocalReadiness, Response};

use crate::daemon_lock::{self, SingleInstance};
use crate::local_model::{
    ActiveReceipt, CandidateHealth, CatalogEntry, InstallClock, InstallConsent, InstallIo,
    InstallRequest, MaintenanceError, MaintenanceKind, MaintenanceReservation, ModelStore,
    ProductionHttps, ReceiptError, bakeoff_winner, install_entry, models_dir, parse_receipt,
    retained_receipt_path, shipped_catalog, store_receipt_atomic, verify_candidate,
};
use crate::setup::WizardIo;

const NO_PRODUCTION_DOWNLOAD: &str = "no bakeoff winner; production weights are not downloaded";

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
    /// Production offers the elected pilot winner for consented download.
    /// Tests may offer a consented fixture instead.
    fn consented_install(&self) -> Option<InstallConsent> {
        let winner = bakeoff_winner(&shipped_catalog())?;
        Some(InstallConsent {
            bytes: winner.total_bytes(),
            license_spdx: winner.license.spdx.to_owned(),
        })
    }
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
        let Some(entry) = bakeoff_winner(&shipped_catalog()) else {
            return Err(NO_PRODUCTION_DOWNLOAD.into());
        };
        if consent.bytes != entry.total_bytes() || consent.license_spdx != entry.license.spdx {
            return Err("consent does not match the catalog entry".into());
        }
        let mut store = load_store()?;
        install_entry(InstallRequest {
            store: &mut store,
            entry,
            fetcher: &ProductionHttps,
            health: &CandidateHealth::default(),
            maintenance: &SetupMaintenance::default(),
            consent,
            io: InstallIo::default(),
            clock: InstallClock::default(),
        })
        .map(|receipt| receipt.catalog_id)
        .map_err(|error| format!("{error:?}"))
    }
}

struct SetupMaintenance {
    lifetime: Mutex<Option<SingleInstance>>,
}

impl Default for SetupMaintenance {
    fn default() -> Self {
        Self {
            lifetime: Mutex::new(None),
        }
    }
}

impl MaintenanceReservation for SetupMaintenance {
    fn acquire(&self) -> Result<MaintenanceKind, MaintenanceError> {
        match daemon_lock::try_acquire_lifetime_lock() {
            Ok(lock) => {
                // The lock, not the socket, is proof of absence. A leftover
                // socket must not be treated as a live daemon.
                unlink_stale_daemon_socket();
                *self
                    .lifetime
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(lock);
                Ok(MaintenanceKind::Idle)
            }
            Err(_) => match crate::system::daemon_status_response() {
                Some(response) => maintenance_from_status(&response),
                None => Err(MaintenanceError::Busy("daemon is not idle")),
            },
        }
    }
}

fn unlink_stale_daemon_socket() {
    let Ok(path) = voisu_core::socket_path() else {
        return;
    };
    let _ = std::fs::remove_file(path);
}

fn maintenance_from_status(response: &Response) -> Result<MaintenanceKind, MaintenanceError> {
    match response.state {
        Some(DaemonState::Recording) => Err(MaintenanceError::Busy("Recording active")),
        Some(DaemonState::Processing)
            if response.message.to_ascii_lowercase().contains("replay") =>
        {
            Err(MaintenanceError::Busy("Replay active"))
        }
        Some(DaemonState::Processing) => Err(MaintenanceError::Busy("daemon is not idle")),
        _ => match response.asr_mode.as_ref().map(|asr| &asr.local_readiness) {
            Some(LocalReadiness::Busy | LocalReadiness::Stopping) => {
                Err(MaintenanceError::Busy("Replay active"))
            }
            Some(LocalReadiness::Loading | LocalReadiness::Verifying) => {
                Err(MaintenanceError::Busy("daemon is not idle"))
            }
            _ => Ok(MaintenanceKind::Idle),
        },
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
    match bakeoff_winner(&shipped_catalog()) {
        Some(winner) => io.writeln(&format!(
            "Catalog (Arch pilot winner: {}; Fedora product support remains evidence-gated):",
            winner.id
        )),
        None => io.writeln("Catalog (no bakeoff winner is selected):"),
    }
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
    let Some(consent) = actions.consented_install() else {
        io.writeln(NO_PRODUCTION_DOWNLOAD);
        return Ok(LocalSetupOutcome::Skipped);
    };
    io.writeln(&format!(
        "Candidate is {} bytes, license {}.",
        consent.bytes, consent.license_spdx
    ));
    if !ask_yes_no(
        io,
        "Download and install this catalog entry? Type yes to consent.",
        false,
    ) {
        io.writeln("No download. Setup did not install a model.");
        return Ok(LocalSetupOutcome::Skipped);
    }
    let id = actions.install_fixture(consent)?;
    io.writeln(&format!("Installed {id} after explicit consent."));
    Ok(LocalSetupOutcome::Installed)
}

#[must_use]
pub fn catalog_presentation() -> Vec<String> {
    shipped_catalog().entries.iter().map(catalog_line).collect()
}

fn catalog_line(entry: &CatalogEntry) -> String {
    let kind = if Some(entry.id) == bakeoff_winner(&shipped_catalog()).map(|winner| winner.id) {
        "Arch pilot winner; explicit-consent download"
    } else if entry.production_weights {
        "unelected production weights; not downloaded"
    } else {
        "test fixture; not a production download"
    };
    format!(
        "{}  {} bytes  {}  {kind}",
        entry.id,
        entry.total_bytes(),
        entry.license.spdx
    )
}

pub fn restore_retained_receipt() -> Result<String, String> {
    let maintenance = SetupMaintenance::default();
    restore_retained_receipt_in(&load_store()?, &maintenance)
}

fn restore_retained_receipt_in(
    store: &ModelStore,
    maintenance: &dyn MaintenanceReservation,
) -> Result<String, String> {
    refuse_busy_maintenance(maintenance.acquire())?;
    let _lock = store
        .lock_exclusive()
        .map_err(|error| format!("{error:?}"))?;
    let receipt = load_retained_from(store)?;
    let entry = catalog_entry_for(&receipt)?;
    if receipt.runtime_abi != entry.runtime_abi.abi_id
        || receipt.protocol != entry.runtime_abi.protocol
    {
        return Err("retained model ABI does not match the catalog".into());
    }
    let artifact = store.artifact_dir(entry, &receipt.artifact_id);
    if let Err(error) = verify_candidate(entry, &artifact) {
        return Err(format!("retained model failed verification: {error:?}"));
    }
    store_receipt_atomic(store.root(), &receipt).map_err(|error| format!("{error:?}"))?;
    Ok(receipt.catalog_id)
}

fn load_store() -> Result<ModelStore, String> {
    let dir = models_dir().map_err(|error| format!("{error:?}"))?;
    ModelStore::open(dir).map_err(|error| format!("{error:?}"))
}

fn load_retained() -> Result<ActiveReceipt, String> {
    load_retained_from(&load_store()?)
}

fn load_retained_from(store: &ModelStore) -> Result<ActiveReceipt, String> {
    let path = retained_receipt_path(store.root());
    let text =
        std::fs::read_to_string(&path).map_err(|_| "no retained model receipt".to_owned())?;
    parse_receipt(&text).map_err(|error| match error {
        ReceiptError::Missing => "no retained model receipt".to_owned(),
        other => format!("{other:?}"),
    })
}

fn refuse_busy_maintenance(
    acquired: Result<MaintenanceKind, MaintenanceError>,
) -> Result<(), String> {
    match acquired {
        Ok(MaintenanceKind::Idle) => Ok(()),
        Ok(MaintenanceKind::Recording) => Err("Recording active".into()),
        Ok(MaintenanceKind::Replay) => Err("Replay active".into()),
        Ok(MaintenanceKind::DaemonBusy) => Err("daemon is not idle".into()),
        Err(MaintenanceError::Busy(message)) => Err(message.into()),
    }
}

fn catalog_entry_for(receipt: &ActiveReceipt) -> Result<&'static CatalogEntry, String> {
    shipped_catalog()
        .entries
        .iter()
        .find(|entry| entry.id == receipt.catalog_id && entry.revision == receipt.catalog_revision)
        .ok_or_else(|| "retained receipt is not in the shipped catalog".to_owned())
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
    use crate::local_model::{bakeoff_winner, ci_fixture_entry, from_entry};
    use std::os::unix::fs::PermissionsExt;

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

        fn consented_install(&self) -> Option<InstallConsent> {
            let fixture = ci_fixture_entry();
            Some(InstallConsent {
                bytes: fixture.total_bytes(),
                license_spdx: fixture.license.spdx.to_owned(),
            })
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
        assert!(transcript.contains("Arch pilot winner"));
        assert!(transcript.contains("unelected production weights"));
        assert!(!transcript.contains("no bakeoff winner"));
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

    struct IdleReservation;

    impl MaintenanceReservation for IdleReservation {
        fn acquire(&self) -> Result<MaintenanceKind, MaintenanceError> {
            Ok(MaintenanceKind::Idle)
        }
    }

    struct RecordingReservation;

    impl MaintenanceReservation for RecordingReservation {
        fn acquire(&self) -> Result<MaintenanceKind, MaintenanceError> {
            Err(MaintenanceError::Busy("Recording active"))
        }
    }

    fn write_fixture_artifact(store: &ModelStore, artifact_id: &str) {
        let fixture = ci_fixture_entry();
        let dir = store.artifact_dir(fixture, artifact_id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("model.bin"), CatalogEntry::fixture_model_bytes()).unwrap();
        std::fs::write(dir.join("health.pcm"), CatalogEntry::fixture_pcm_bytes()).unwrap();
    }

    #[test]
    fn restore_retained_receipt_rewrites_active_without_fetch() {
        let temp = tempfile::tempdir().unwrap();
        let store = ModelStore::open(temp.path().to_path_buf()).unwrap();
        let fixture = ci_fixture_entry();
        let previous = from_entry(fixture, "old");
        let retained = from_entry(fixture, "kept");
        write_fixture_artifact(&store, "old");
        write_fixture_artifact(&store, "kept");
        store_receipt_atomic(store.root(), &previous).unwrap();
        std::fs::write(
            retained_receipt_path(store.root()),
            crate::local_model::encode_receipt(&retained),
        )
        .unwrap();
        let id = restore_retained_receipt_in(&store, &IdleReservation).unwrap();
        assert_eq!(id, "l3-health-fixture");
        assert_eq!(
            store.load_active().unwrap().unwrap().artifact_id,
            retained.artifact_id
        );
    }

    #[test]
    fn restore_leaves_active_receipt_when_retained_artifact_is_corrupt() {
        let temp = tempfile::tempdir().unwrap();
        let store = ModelStore::open(temp.path().to_path_buf()).unwrap();
        let fixture = ci_fixture_entry();
        let previous = from_entry(fixture, "old");
        let retained = from_entry(fixture, "kept");
        write_fixture_artifact(&store, "old");
        let bad = store.artifact_dir(fixture, "kept");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("model.bin"), b"corrupt-bytes").unwrap();
        std::fs::write(bad.join("health.pcm"), CatalogEntry::fixture_pcm_bytes()).unwrap();
        store_receipt_atomic(store.root(), &previous).unwrap();
        std::fs::write(
            retained_receipt_path(store.root()),
            crate::local_model::encode_receipt(&retained),
        )
        .unwrap();
        let error = restore_retained_receipt_in(&store, &IdleReservation).unwrap_err();
        assert!(error.contains("failed verification"), "{error}");
        assert_eq!(
            store.load_active().unwrap().unwrap().artifact_id,
            previous.artifact_id
        );
    }

    #[test]
    fn production_run_offers_the_pilot_winner_for_consent() {
        let mut io = FakeIo::new(vec![]);
        let mut actions = ProductionLocalSetup;
        let outcome = run_with(&mut io, &mut actions).unwrap();
        assert_eq!(outcome, LocalSetupOutcome::Skipped);
        let transcript = io.transcript();
        let winner = bakeoff_winner(&shipped_catalog()).expect("pilot winner");
        assert!(transcript.contains(winner.id), "{transcript}");
        assert!(transcript.contains("No download"), "{transcript}");
        assert!(
            !transcript.contains("Download and install the catalog fixture"),
            "{transcript}"
        );
        let consent = actions.consented_install().expect("pilot consent");
        assert_eq!(consent.bytes, winner.total_bytes());
        assert_eq!(consent.license_spdx, winner.license.spdx);
        assert!(CLOUD_CREDENTIAL_MAINTENANCE.contains("Cloud credential-maintenance"));
    }

    #[test]
    fn production_adapter_refuses_mismatched_consent() {
        let mut actions = ProductionLocalSetup;
        let error = actions
            .install_fixture(InstallConsent {
                bytes: 1,
                license_spdx: "MIT".into(),
            })
            .unwrap_err();
        assert!(error.contains("consent does not match"), "{error}");
    }

    #[test]
    fn maintenance_rejects_recording_replay_and_busy_daemon() {
        let recording = Response::success(DaemonState::Recording, "Recording");
        assert_eq!(
            maintenance_from_status(&recording),
            Err(MaintenanceError::Busy("Recording active"))
        );
        let replay = Response::success(DaemonState::Processing, "Replay in progress");
        assert_eq!(
            maintenance_from_status(&replay),
            Err(MaintenanceError::Busy("Replay active"))
        );
        let busy = Response::success(DaemonState::Processing, "processing");
        assert_eq!(
            maintenance_from_status(&busy),
            Err(MaintenanceError::Busy("daemon is not idle"))
        );
        let idle = Response::success(DaemonState::Idle, "idle");
        assert_eq!(maintenance_from_status(&idle), Ok(MaintenanceKind::Idle));
    }

    #[test]
    fn restore_fails_closed_when_the_store_lock_is_held() {
        let temp = tempfile::tempdir().unwrap();
        let store = ModelStore::open(temp.path().to_path_buf()).unwrap();
        let _held = store.lock_exclusive().unwrap();
        let error = restore_retained_receipt_in(&store, &IdleReservation).unwrap_err();
        assert!(error.contains("Busy"), "{error}");
    }

    #[test]
    fn restore_is_refused_while_recording() {
        let temp = tempfile::tempdir().unwrap();
        let store = ModelStore::open(temp.path().to_path_buf()).unwrap();
        let error = restore_retained_receipt_in(&store, &RecordingReservation).unwrap_err();
        assert_eq!(error, "Recording active");
        assert!(store.load_active().unwrap().is_none());
    }

    #[test]
    fn restore_holds_lifetime_lock_when_daemon_is_absent() {
        let runtime = tempfile::tempdir().unwrap();
        std::fs::set_permissions(runtime.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let _guard = RuntimeGuard::set(runtime.path());
        let store_dir = tempfile::tempdir().unwrap();
        let store = ModelStore::open(store_dir.path().to_path_buf()).unwrap();
        let fixture = ci_fixture_entry();
        let previous = from_entry(fixture, "old");
        let retained = from_entry(fixture, "kept");
        write_fixture_artifact(&store, "old");
        write_fixture_artifact(&store, "kept");
        store_receipt_atomic(store.root(), &previous).unwrap();
        std::fs::write(
            retained_receipt_path(store.root()),
            crate::local_model::encode_receipt(&retained),
        )
        .unwrap();
        let maintenance = SetupMaintenance::default();
        let id = restore_retained_receipt_in(&store, &maintenance).unwrap();
        assert_eq!(id, "l3-health-fixture");
        assert!(
            daemon_lock::try_acquire_lifetime_lock().is_err(),
            "restore must keep the daemon from starting through verify+write"
        );
        drop(maintenance);
        assert!(daemon_lock::try_acquire_lifetime_lock().is_ok());
    }

    #[test]
    fn acquire_ignores_a_stale_socket_when_the_lifetime_lock_is_free() {
        let runtime = tempfile::tempdir().unwrap();
        std::fs::set_permissions(runtime.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let _guard = RuntimeGuard::set(runtime.path());
        let lock_path = daemon_lock::lock_path().unwrap();
        daemon_lock::create_private_runtime_dirs(lock_path.parent().unwrap()).unwrap();
        let socket = voisu_core::socket_path().unwrap();
        let _stale = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let maintenance = SetupMaintenance::default();
        assert_eq!(maintenance.acquire(), Ok(MaintenanceKind::Idle));
        assert!(
            !socket.exists(),
            "stale socket must be unlinked after lock success"
        );
        assert!(daemon_lock::try_acquire_lifetime_lock().is_err());
    }

    #[test]
    fn setup_maintenance_holds_the_lifetime_lock_when_the_daemon_is_absent() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let _guard = RuntimeGuard::set(temp.path());
        let maintenance = SetupMaintenance::default();
        assert_eq!(maintenance.acquire(), Ok(MaintenanceKind::Idle));
        assert!(
            daemon_lock::try_acquire_lifetime_lock().is_err(),
            "install must keep the daemon from starting"
        );
        drop(maintenance);
        assert!(daemon_lock::try_acquire_lifetime_lock().is_ok());
    }

    fn runtime_env_lock() -> &'static Mutex<()> {
        static LOCK: Mutex<()> = Mutex::new(());
        &LOCK
    }

    struct RuntimeGuard {
        previous: Option<std::ffi::OsString>,
        _env: std::sync::MutexGuard<'static, ()>,
    }

    impl RuntimeGuard {
        fn set(path: &std::path::Path) -> Self {
            let env = runtime_env_lock()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let previous = std::env::var_os("XDG_RUNTIME_DIR");
            unsafe {
                std::env::set_var("XDG_RUNTIME_DIR", path);
            }
            Self {
                previous,
                _env: env,
            }
        }
    }

    impl Drop for RuntimeGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.previous {
                    Some(value) => std::env::set_var("XDG_RUNTIME_DIR", value),
                    None => std::env::remove_var("XDG_RUNTIME_DIR"),
                }
            }
        }
    }
}
