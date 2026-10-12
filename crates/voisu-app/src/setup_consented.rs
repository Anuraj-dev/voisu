//! Consented Install offer during `voisu setup`, and `voisu local remove`.
//!
//! The offer runs after Local is selected and before `run_consented_local_setup`
//! (the closed Bakeoff-catalog repair flow). That older flow must not offer a
//! second download; it stays evidence-gated. This offer is the only setup path
//! that can fetch, and only after an explicit yes.

use voisu_core::AsrMode;

use crate::asr_mode::LOCAL_UNAVAILABLE;
use crate::local_model::{
    CONSENT_TEXT, CONSENTED_ENTRY, ConsentedConsent, ConsentedError, ConsentedInstallRequest,
    HostHardware, InstallClock, InstallIo, ModelStore, OfflineRuntimeBuilder, ProductionHttps,
    models_dir_from_state, remove_consented_install, suggest_local, verify_consented_install,
};
use crate::local_setup::SetupMaintenance;
use crate::local_worker::ConsentedWorkerHealth;
use crate::setup::{SETUP_COMPLETE_MESSAGE, WizardIo};
use crate::setup_engine::{ConsentedStatus, ask_yes_no, distil_is_suggested};

pub use crate::local_model::{DEVELOPMENT_ROOT_NAME, SystemCommandRunner, probe_hardware};

const INSTALL_PROMPT: &str = "Download and install distil-large-v3.5-int8 now?";
const INSTALL_DURATION_HINT: &str = "This can take several minutes.";
const INSTALL_SUCCESS: &str = "Installed distil-large-v3.5-int8 (Consented Install).";
const INSTALL_RETRY: &str =
    "Run `voisu setup` to retry or `voisu mode cloud` to dictate with Cloud.";
const REMOVE_CONFIRM: &str = "Remove distil-large-v3.5-int8 (Consented Install)?";
const REMOVE_USAGE: &str = "usage: voisu local remove [--yes]";
pub const REMOVE_REQUIRES_YES: &str = "Non-interactive `voisu local remove` requires `--yes`.";
const REMOVED: &str = "Removed distil-large-v3.5-int8 (Consented Install).";
const NOTHING_TO_REMOVE: &str = "No Consented Install is installed.";
const IN_USE: &str =
    "The Consented Install is in use; stop dictation or run `voisu service stop` first.";

/// Outcome of the setup Consented Install offer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConsentedInstallStep {
    Skipped,
    Declined,
    Installed,
    Failed,
}

/// `voisu local` subcommands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalCommand {
    Remove { yes: bool },
}

/// Result of an injected remove seam.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoveOutcome {
    Removed,
    Absent,
}

/// CLI result for `voisu local remove`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LocalRemoveStatus {
    Success,
    Usage(&'static str),
    Failed(String),
}

/// Prints CONSENT_TEXT and installs only after an explicit yes.
///
/// Non-interactive, parakeet, and an already-verified install never fetch.
pub fn run_consented_install_step(
    io: &mut dyn WizardIo,
    hw: &HostHardware,
    interactive: bool,
    installed: bool,
    now_unix: u64,
    install: impl FnOnce(ConsentedConsent) -> Result<(), ConsentedError>,
) -> ConsentedInstallStep {
    if installed || !interactive || !distil_is_suggested(&suggest_local(hw)) {
        return ConsentedInstallStep::Skipped;
    }
    io.writeln(CONSENT_TEXT);
    match ask_yes_no(io, INSTALL_PROMPT, false) {
        Some(true) => {
            io.writeln(INSTALL_DURATION_HINT);
            match install(ConsentedConsent::accepted(now_unix)) {
                Ok(()) => {
                    io.writeln(INSTALL_SUCCESS);
                    ConsentedInstallStep::Installed
                }
                Err(error) => {
                    io.writeln(&format!("Install failed: {}", consented_error_line(&error)));
                    io.writeln(LOCAL_UNAVAILABLE);
                    io.writeln(INSTALL_RETRY);
                    ConsentedInstallStep::Failed
                }
            }
        }
        Some(false) | None => ConsentedInstallStep::Declined,
    }
}

#[must_use]
pub fn local_complete_message(verified_consented: bool, development_pilot: bool) -> String {
    if verified_consented || development_pilot {
        SETUP_COMPLETE_MESSAGE.to_owned()
    } else {
        format!("{LOCAL_UNAVAILABLE}\nRun `voisu mode cloud` to dictate with Cloud.")
    }
}

pub fn parse_local_command(args: &[String]) -> Result<LocalCommand, String> {
    match args {
        [action] if action == "remove" => Ok(LocalCommand::Remove { yes: false }),
        [action, flag] if action == "remove" && flag == "--yes" => {
            Ok(LocalCommand::Remove { yes: true })
        }
        _ => Err(REMOVE_USAGE.to_owned()),
    }
}

pub fn run_local_remove(
    io: &mut dyn WizardIo,
    interactive: bool,
    command: LocalCommand,
    mode: AsrMode,
    remove: impl FnOnce() -> Result<RemoveOutcome, ConsentedError>,
) -> LocalRemoveStatus {
    let LocalCommand::Remove { yes } = command;
    if !yes {
        if !interactive {
            return LocalRemoveStatus::Usage(REMOVE_REQUIRES_YES);
        }
        match ask_yes_no(io, REMOVE_CONFIRM, false) {
            Some(true) => {}
            Some(false) | None => return LocalRemoveStatus::Success,
        }
    }
    match remove() {
        Ok(RemoveOutcome::Removed) => {
            io.writeln(REMOVED);
            if mode == AsrMode::Local {
                io.writeln(LOCAL_UNAVAILABLE);
            }
            LocalRemoveStatus::Success
        }
        Ok(RemoveOutcome::Absent) => {
            io.writeln(NOTHING_TO_REMOVE);
            if mode == AsrMode::Local {
                io.writeln(LOCAL_UNAVAILABLE);
            }
            LocalRemoveStatus::Success
        }
        Err(ConsentedError::Leased) | Err(ConsentedError::Maintenance) => {
            LocalRemoveStatus::Failed(IN_USE.to_owned())
        }
        Err(error) => LocalRemoveStatus::Failed(consented_error_line(&error)),
    }
}

#[must_use]
pub fn consented_error_line(error: &ConsentedError) -> String {
    match error {
        ConsentedError::NoSpace => "Not enough free disk space.".into(),
        ConsentedError::Maintenance => IN_USE.into(),
        ConsentedError::Health(_) => "The installed model failed its health check.".into(),
        ConsentedError::HealthUnreaped => {
            "The health-check worker did not stop; its files were kept.".into()
        }
        ConsentedError::Fetch(_) | ConsentedError::Deadline => {
            "The download failed (network or deadline).".into()
        }
        ConsentedError::Leased => IN_USE.into(),
        ConsentedError::Consent => "Consent did not match the catalog.".into(),
        ConsentedError::Missing => NOTHING_TO_REMOVE.into(),
        ConsentedError::Receipt => "The Consented Install receipt is invalid.".into(),
        ConsentedError::Digest(_) => "A downloaded file failed its checksum.".into(),
        ConsentedError::Tree(_) => "The install tree is invalid.".into(),
        ConsentedError::Build(_) => "The Python/CUDA runtime failed to build.".into(),
        ConsentedError::Store(_) => "The model store is busy or unavailable.".into(),
        ConsentedError::Fs(_) | ConsentedError::Io(_) => {
            "A filesystem error stopped the operation.".into()
        }
        ConsentedError::Aborted => "The install was aborted.".into(),
    }
}

/// Real-store probe for the setup binary. Does not create a models directory
/// when none exists.
#[must_use]
pub fn production_consented_status() -> ConsentedStatus {
    let dir =
        match models_dir_from_state(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME")) {
            Ok(dir) => dir,
            Err(_) => {
                return ConsentedStatus::Unknown("the model store path is unavailable".into());
            }
        };
    if !dir.exists() {
        return ConsentedStatus::NotInstalled;
    }
    let store = match ModelStore::open(dir) {
        Ok(store) => store,
        Err(_) => return ConsentedStatus::Unknown("the model store is unavailable".into()),
    };
    match verify_consented_install(&store) {
        Ok(_) => ConsentedStatus::Installed,
        Err(ConsentedError::Missing) => ConsentedStatus::NotInstalled,
        Err(error) => ConsentedStatus::Unknown(consented_error_line(&error)),
    }
}

/// Completes Local setup from the real store and the developer-pilot flag.
#[must_use]
pub fn production_local_complete_message() -> String {
    local_complete_message(
        matches!(production_consented_status(), ConsentedStatus::Installed),
        crate::local_model::development_local_enabled(),
    )
}

/// Fetches and installs only when the setup offer calls this after an explicit yes.
pub fn production_install(consent: ConsentedConsent) -> Result<(), ConsentedError> {
    let models =
        models_dir_from_state(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
            .map_err(ConsentedError::Store)?;
    let store = ModelStore::open(models).map_err(ConsentedError::Store)?;
    let state = voisu_core::state_dir()
        .map_err(|message| ConsentedError::Io(std::io::Error::other(message)))?;
    let fetcher = ProductionHttps;
    let builder = OfflineRuntimeBuilder;
    let health = ConsentedWorkerHealth::new(state);
    let maintenance = SetupMaintenance::default();
    crate::local_model::install_consented_install(ConsentedInstallRequest {
        store: &store,
        consent,
        fetcher: &fetcher,
        builder: &builder,
        health: &health,
        maintenance: &maintenance,
        io: InstallIo::default(),
        clock: InstallClock::default(),
    })
    .map(|_| ())
}

pub fn production_remove() -> Result<RemoveOutcome, ConsentedError> {
    let dir = models_dir_from_state(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
        .map_err(ConsentedError::Store)?;
    if !dir.exists() {
        return Ok(RemoveOutcome::Absent);
    }
    let store = ModelStore::open(dir).map_err(ConsentedError::Store)?;
    match verify_consented_install(&store) {
        Err(ConsentedError::Missing) => Ok(RemoveOutcome::Absent),
        Err(error) => Err(error),
        Ok(verified) => {
            drop(verified);
            remove_consented_install(&store)?;
            Ok(RemoveOutcome::Removed)
        }
    }
}

const _: () = assert!(CONSENTED_ENTRY.consent_installable);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_model::{DISTIL_MODEL_ID, FetchError, GpuProbe, NvidiaGpu};
    use crate::setup_engine::{
        ConsentedStatus, EngineChoice, INSTALLED_AVAILABILITY, run_engine_screen,
    };
    use voisu_core::KeyDiagnosis;

    struct FakeIo {
        lines: Vec<Option<String>>,
        output: Vec<String>,
        prompts: usize,
        install_calls: Vec<ConsentedConsent>,
    }

    impl FakeIo {
        fn new(lines: Vec<Option<&str>>) -> Self {
            Self {
                lines: lines
                    .into_iter()
                    .map(|line| line.map(str::to_owned))
                    .collect(),
                output: Vec::new(),
                prompts: 0,
                install_calls: Vec::new(),
            }
        }

        fn transcript(&self) -> String {
            self.output.join("\n")
        }
    }

    impl WizardIo for FakeIo {
        fn writeln(&mut self, line: &str) {
            self.output.push(line.to_owned());
        }

        fn prompt_line(&mut self, prompt: &str) -> Option<String> {
            self.prompts += 1;
            self.output.push(format!("PROMPT {prompt}"));
            if self.lines.is_empty() {
                None
            } else {
                self.lines.remove(0)
            }
        }

        fn prompt_secret(&mut self, _prompt: &str) -> Option<String> {
            panic!("Consented Install must not prompt for Cloud keys");
        }
    }

    fn gpu(index: u32, total_vram_mib: u64) -> NvidiaGpu {
        NvidiaGpu {
            index,
            name: "RTX 5050 Laptop".into(),
            total_vram_mib,
            driver_version: "580.95".into(),
        }
    }

    fn host(gpu: GpuProbe) -> HostHardware {
        HostHardware {
            gpu,
            ram_bytes: Some(16 * 1024 * 1024 * 1024),
            cpu_cores: 16,
        }
    }

    const NOW: u64 = 1_700_000_000;

    fn offer(
        hw: &HostHardware,
        interactive: bool,
        installed: bool,
        answers: Vec<Option<&str>>,
        result: Result<(), ConsentedError>,
    ) -> (ConsentedInstallStep, FakeIo) {
        let mut io = FakeIo::new(answers);
        let mut recorded = Vec::new();
        let step =
            run_consented_install_step(&mut io, hw, interactive, installed, NOW, |consent| {
                recorded.push(consent);
                match &result {
                    Ok(()) => Ok(()),
                    Err(ConsentedError::NoSpace) => Err(ConsentedError::NoSpace),
                    Err(ConsentedError::Maintenance) => Err(ConsentedError::Maintenance),
                    Err(ConsentedError::Health(message)) => {
                        Err(ConsentedError::Health(message.clone()))
                    }
                    Err(ConsentedError::Fetch(error)) => Err(ConsentedError::Fetch(error.clone())),
                    Err(_) => Err(ConsentedError::NoSpace),
                }
            });
        io.install_calls = recorded;
        (step, io)
    }

    #[test]
    fn rtx_5050_offers_install_and_decline_never_fetches() {
        let hw = host(GpuProbe::Nvidia(vec![gpu(0, 8151)]));
        let (step, io) = offer(&hw, true, false, vec![Some("n")], Ok(()));
        assert_eq!(step, ConsentedInstallStep::Declined);
        assert!(
            io.transcript().contains(CONSENT_TEXT),
            "{}",
            io.transcript()
        );
        assert!(
            io.transcript()
                .contains("Download and install distil-large-v3.5-int8 now? [y/N]"),
            "{}",
            io.transcript()
        );
        assert!(io.install_calls.is_empty(), "{:?}", io.install_calls);
        assert!(
            !io.transcript().contains(INSTALL_DURATION_HINT),
            "{}",
            io.transcript()
        );
    }

    #[test]
    fn rtx_5050_yes_installs_once_with_exact_consent() {
        let hw = host(GpuProbe::Nvidia(vec![gpu(0, 8151)]));
        let (step, io) = offer(&hw, true, false, vec![Some("y")], Ok(()));
        assert_eq!(step, ConsentedInstallStep::Installed);
        assert_eq!(io.install_calls, vec![ConsentedConsent::accepted(NOW)]);
        assert_eq!(io.install_calls[0].text, CONSENT_TEXT);
        assert_eq!(
            io.install_calls[0].license_spdx,
            CONSENTED_ENTRY.license_spdx
        );
        assert!(
            io.transcript().contains(INSTALL_DURATION_HINT),
            "{}",
            io.transcript()
        );
        assert!(
            io.transcript().contains(INSTALL_SUCCESS),
            "{}",
            io.transcript()
        );
    }

    #[test]
    fn small_gpu_and_absent_nvidia_never_offer_install() {
        for hw in [
            host(GpuProbe::Nvidia(vec![gpu(0, 2048)])),
            host(GpuProbe::NoneFound),
        ] {
            let leftover = vec![Some("y")];
            let (step, io) = offer(&hw, true, false, leftover, Ok(()));
            assert_eq!(step, ConsentedInstallStep::Skipped);
            assert_eq!(io.prompts, 0);
            assert!(io.install_calls.is_empty());
            assert!(
                !io.transcript().contains(CONSENT_TEXT),
                "{}",
                io.transcript()
            );
        }
    }

    #[test]
    fn verified_install_does_not_offer_again() {
        let hw = host(GpuProbe::Nvidia(vec![gpu(0, 8151)]));
        let mut engine = FakeIo::new(vec![]);
        let choice = run_engine_screen(
            &mut engine,
            AsrMode::Cloud,
            &hw,
            &KeyDiagnosis::Absent,
            &KeyDiagnosis::Absent,
            false,
            |_| false,
            ConsentedStatus::Installed,
        );
        assert_eq!(choice, EngineChoice::Keep(AsrMode::Cloud));
        assert!(
            engine.transcript().contains(INSTALLED_AVAILABILITY),
            "{}",
            engine.transcript()
        );
        let (step, io) = offer(&hw, true, true, vec![Some("y")], Ok(()));
        assert_eq!(step, ConsentedInstallStep::Skipped);
        assert!(io.install_calls.is_empty());
        assert_eq!(io.prompts, 0);
    }

    #[test]
    fn install_errors_print_the_failure_lines() {
        let hw = host(GpuProbe::Nvidia(vec![gpu(0, 8151)]));
        let cases = [
            (
                ConsentedError::NoSpace,
                "Install failed: Not enough free disk space.",
            ),
            (
                ConsentedError::Maintenance,
                "Install failed: The Consented Install is in use; stop dictation or run `voisu service stop` first.",
            ),
            (
                ConsentedError::Fetch(FetchError::Io("reset".into())),
                "Install failed: The download failed (network or deadline).",
            ),
            (
                ConsentedError::Health("cuda".into()),
                "Install failed: The installed model failed its health check.",
            ),
        ];
        for (error, line) in cases {
            let (step, io) = offer(&hw, true, false, vec![Some("yes")], Err(error));
            assert_eq!(step, ConsentedInstallStep::Failed);
            let text = io.transcript();
            assert!(text.contains(line), "{line}: {text}");
            assert!(text.contains(LOCAL_UNAVAILABLE), "{text}");
            assert!(text.contains(INSTALL_RETRY), "{text}");
            assert_eq!(io.install_calls, vec![ConsentedConsent::accepted(NOW)]);
        }
    }

    #[test]
    fn non_interactive_never_prompts_or_installs() {
        let hw = host(GpuProbe::Nvidia(vec![gpu(0, 8151)]));
        let leftover = vec![Some("y")];
        let (step, io) = offer(&hw, false, false, leftover, Ok(()));
        assert_eq!(step, ConsentedInstallStep::Skipped);
        assert_eq!(io.prompts, 0);
        assert!(io.install_calls.is_empty());
        assert!(
            !io.transcript().contains("Download and install"),
            "{}",
            io.transcript()
        );
        assert!(
            !io.transcript().contains(CONSENT_TEXT),
            "{}",
            io.transcript()
        );
    }

    #[test]
    fn eof_and_enter_are_declines() {
        let hw = host(GpuProbe::Nvidia(vec![gpu(0, 8151)]));
        for answers in [vec![], vec![Some("")]] {
            let (step, io) = offer(&hw, true, false, answers, Ok(()));
            assert_eq!(step, ConsentedInstallStep::Declined);
            assert!(io.install_calls.is_empty());
        }
    }

    #[test]
    fn rtx_5050_setup_transcript_decline_and_accept() {
        let hw = host(GpuProbe::Nvidia(vec![gpu(0, 8151)]));
        let mut decline = FakeIo::new(vec![Some("2"), Some("y"), Some("n")]);
        let choice = run_engine_screen(
            &mut decline,
            AsrMode::Cloud,
            &hw,
            &KeyDiagnosis::Absent,
            &KeyDiagnosis::Absent,
            true,
            |_| false,
            ConsentedStatus::NotInstalled,
        );
        assert_eq!(choice, EngineChoice::Switch(AsrMode::Local));
        let step = run_consented_install_step(&mut decline, &hw, true, false, NOW, |_| {
            panic!("decline must not install")
        });
        assert_eq!(step, ConsentedInstallStep::Declined);
        let declined = decline.transcript();
        assert!(
            declined.contains("Suggested for this hardware"),
            "{declined}"
        );
        assert!(declined.contains(DISTIL_MODEL_ID), "{declined}");
        assert!(declined.contains(CONSENT_TEXT), "{declined}");
        assert!(
            declined.contains("Download and install distil-large-v3.5-int8 now? [y/N]"),
            "{declined}"
        );
        assert!(!declined.contains(INSTALL_SUCCESS), "{declined}");

        let mut accept = FakeIo::new(vec![Some("2"), Some("y"), Some("y")]);
        let choice = run_engine_screen(
            &mut accept,
            AsrMode::Cloud,
            &hw,
            &KeyDiagnosis::Absent,
            &KeyDiagnosis::Absent,
            true,
            |_| false,
            ConsentedStatus::NotInstalled,
        );
        assert_eq!(choice, EngineChoice::Switch(AsrMode::Local));
        let mut consents = Vec::new();
        let step = run_consented_install_step(&mut accept, &hw, true, false, NOW, |consent| {
            consents.push(consent);
            Ok(())
        });
        assert_eq!(step, ConsentedInstallStep::Installed);
        assert_eq!(consents, vec![ConsentedConsent::accepted(NOW)]);
        let accepted = accept.transcript();
        assert!(accepted.contains(CONSENT_TEXT), "{accepted}");
        assert!(accepted.contains(INSTALL_DURATION_HINT), "{accepted}");
        assert!(accepted.contains(INSTALL_SUCCESS), "{accepted}");
    }

    #[test]
    fn complete_message_treats_a_verified_install_as_setup_complete() {
        assert_eq!(local_complete_message(true, false), SETUP_COMPLETE_MESSAGE);
        assert_eq!(local_complete_message(false, true), SETUP_COMPLETE_MESSAGE);
        assert_eq!(
            local_complete_message(false, false),
            format!("{LOCAL_UNAVAILABLE}\nRun `voisu mode cloud` to dictate with Cloud.")
        );
    }

    #[test]
    fn parse_local_remove_and_yes() {
        assert_eq!(
            parse_local_command(&["remove".into()]).unwrap(),
            LocalCommand::Remove { yes: false }
        );
        assert_eq!(
            parse_local_command(&["remove".into(), "--yes".into()]).unwrap(),
            LocalCommand::Remove { yes: true }
        );
        assert_eq!(parse_local_command(&[]).unwrap_err(), REMOVE_USAGE);
        assert_eq!(
            parse_local_command(&["remove".into(), "--yes".into(), "extra".into()]).unwrap_err(),
            REMOVE_USAGE
        );
        assert_eq!(
            parse_local_command(&["install".into()]).unwrap_err(),
            REMOVE_USAGE
        );
    }

    #[test]
    fn local_remove_interactive_confirm_defaults_to_no() {
        let mut io = FakeIo::new(vec![Some("")]);
        let mut removed = false;
        let status = run_local_remove(
            &mut io,
            true,
            LocalCommand::Remove { yes: false },
            AsrMode::Local,
            || {
                removed = true;
                Ok(RemoveOutcome::Removed)
            },
        );
        assert_eq!(status, LocalRemoveStatus::Success);
        assert!(!removed);
        assert!(
            io.transcript()
                .contains("Remove distil-large-v3.5-int8 (Consented Install)? [y/N]"),
            "{}",
            io.transcript()
        );
        assert!(!io.transcript().contains(REMOVED), "{}", io.transcript());
    }

    #[test]
    fn local_remove_non_interactive_requires_yes() {
        let mut io = FakeIo::new(vec![]);
        let mut removed = false;
        let status = run_local_remove(
            &mut io,
            false,
            LocalCommand::Remove { yes: false },
            AsrMode::Cloud,
            || {
                removed = true;
                Ok(RemoveOutcome::Removed)
            },
        );
        assert_eq!(status, LocalRemoveStatus::Usage(REMOVE_REQUIRES_YES));
        assert!(!removed);
        assert_eq!(io.prompts, 0);
    }

    #[test]
    fn local_remove_yes_prints_what_was_removed() {
        let mut io = FakeIo::new(vec![]);
        let status = run_local_remove(
            &mut io,
            false,
            LocalCommand::Remove { yes: true },
            AsrMode::Local,
            || Ok(RemoveOutcome::Removed),
        );
        assert_eq!(status, LocalRemoveStatus::Success);
        let text = io.transcript();
        assert!(text.contains(REMOVED), "{text}");
        assert!(text.contains(LOCAL_UNAVAILABLE), "{text}");
        assert_eq!(io.prompts, 0);
    }

    #[test]
    fn local_remove_leased_tells_the_user_to_stop_the_service() {
        let mut io = FakeIo::new(vec![]);
        let status = run_local_remove(
            &mut io,
            false,
            LocalCommand::Remove { yes: true },
            AsrMode::Local,
            || Err(ConsentedError::Leased),
        );
        assert_eq!(status, LocalRemoveStatus::Failed(IN_USE.to_owned()));
        assert!(!io.transcript().contains(REMOVED), "{}", io.transcript());
    }
}
