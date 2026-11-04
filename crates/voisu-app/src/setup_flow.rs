//! `voisu setup`, start to finish: one flow shared by the text wizard and the
//! terminal interface. The flow decides what happens in which order; each
//! question it asks goes through [`SetupQuestions`], so the text wizard prints
//! and reads lines while the interface shows screens, and both make the same
//! choices from the same facts.

use std::path::Path;

use voisu_core::{AsrMode, KeyDiagnosis, Provider, SecretStore};

use crate::local_model::HostHardware;
use crate::setup::{
    HyprlandSetupOutcome, LiveHyprlandSetupActions, LiveKeyValidator, ProviderOutcome,
    SETUP_COMPLETE_MESSAGE, SetupOutcome, StdioWizard, WizardIo, run_consented_local_setup,
    run_hyprland_setup, run_setup,
};
use crate::setup_consented::{
    ConsentedInstallStep, SystemCommandRunner, ask_cloud_after_local,
    developer_pilot_manifest_exists, probe_hardware, production_cloud_declined_marker,
    production_consented_status, production_install, production_local_complete_message,
    production_restart_daemon, run_catalog_repair_if_needed, run_consented_install_step,
};
use crate::setup_engine::{
    ConsentedStatus, EngineChoice, consent_confirms_local_switch, run_engine_screen,
};
use crate::setup_profile::{
    HyprlandConfig, SetupProfile, discover_setup_profile, live_setup_facts,
};
use crate::system::SecretToolStore;

/// Exit code for a setup that could not finish.
pub const SETUP_INCOMPLETE: u8 = 4;

/// What the engine question is decided from.
pub struct EngineFacts<'a> {
    pub current: AsrMode,
    pub hardware: &'a HostHardware,
    pub deepgram: &'a KeyDiagnosis,
    pub groq: &'a KeyDiagnosis,
    pub interactive: bool,
    pub consented: &'a ConsentedStatus,
}

/// The questions setup asks, and where its words go. Every interactive step of
/// [`run`] is one method, so a front end answers whole steps rather than
/// individual prompts.
pub trait SetupQuestions {
    /// One line of progress or result.
    fn say(&mut self, line: &str);
    /// The reason setup stops. [`run`] returns the exit code after this.
    fn fail(&mut self, message: &str);
    /// Cloud or Local.
    fn choose_engine(&mut self, facts: &EngineFacts<'_>) -> EngineChoice;
    /// Offers the Local download (never without consent) and installs it.
    fn install_local(
        &mut self,
        hardware: &HostHardware,
        interactive: bool,
        installed: bool,
        now_unix: u64,
    ) -> ConsentedInstallStep;
    /// The model status and repair step that follows a Local install that did
    /// not happen.
    fn repair_local(&mut self, step: ConsentedInstallStep, installed: bool) -> Result<(), String>;
    /// Whether a Local user also wants Cloud keys.
    fn cloud_after_local(
        &mut self,
        interactive: bool,
        deepgram: &KeyDiagnosis,
        groq: &KeyDiagnosis,
        declined_marker: Option<&Path>,
    ) -> bool;
    /// The Groq and Deepgram keys: keep, enter, check and store each one.
    fn cloud_keys(&mut self) -> SetupOutcome;
    /// Delivery, the Trigger Key and the services on Hyprland.
    fn hyprland(&mut self, config: &Path) -> Result<HyprlandSetupOutcome, String>;
}

/// The text wizard: plain lines on stdout, the stop reason on stderr, answers
/// read from stdin.
pub struct TextSetup;

impl SetupQuestions for TextSetup {
    fn say(&mut self, line: &str) {
        StdioWizard.writeln(line);
    }

    fn fail(&mut self, message: &str) {
        eprintln!("{message}");
    }

    fn choose_engine(&mut self, facts: &EngineFacts<'_>) -> EngineChoice {
        run_engine_screen(
            &mut StdioWizard,
            facts.current,
            facts.hardware,
            facts.deepgram,
            facts.groq,
            facts.interactive,
            developer_pilot_manifest_exists,
            facts.consented.clone(),
        )
    }

    fn install_local(
        &mut self,
        hardware: &HostHardware,
        interactive: bool,
        installed: bool,
        now_unix: u64,
    ) -> ConsentedInstallStep {
        run_consented_install_step(
            &mut StdioWizard,
            hardware,
            interactive,
            installed,
            now_unix,
            production_install,
            production_restart_daemon,
        )
    }

    fn repair_local(&mut self, step: ConsentedInstallStep, installed: bool) -> Result<(), String> {
        run_catalog_repair_if_needed(&mut StdioWizard, step, installed, run_consented_local_setup)
    }

    fn cloud_after_local(
        &mut self,
        interactive: bool,
        deepgram: &KeyDiagnosis,
        groq: &KeyDiagnosis,
        declined_marker: Option<&Path>,
    ) -> bool {
        ask_cloud_after_local(
            &mut StdioWizard,
            interactive,
            deepgram,
            groq,
            declined_marker,
        )
    }

    fn cloud_keys(&mut self) -> SetupOutcome {
        run_setup(
            &mut StdioWizard,
            &mut SecretToolStore,
            &mut LiveKeyValidator,
        )
    }

    fn hyprland(&mut self, config: &Path) -> Result<HyprlandSetupOutcome, String> {
        run_hyprland_setup(
            config,
            &mut LiveHyprlandSetupActions {
                io: &mut StdioWizard,
            },
        )
    }
}

/// Runs setup and returns its exit code. `wizard_only` (integration tests)
/// stops after the questions, before any desktop or service change.
pub fn run(q: &mut dyn SetupQuestions, interactive: bool, wizard_only: bool) -> u8 {
    let discovery = match discover_setup_profile(&live_setup_facts()) {
        Ok(discovery) => discovery,
        Err(error) => return stop(q, &error.message()),
    };
    q.say(&format!(
        "Detected {} Setup Profile.",
        discovery.profile.as_str()
    ));
    let current = if crate::local_setup::is_local_mode() {
        AsrMode::Local
    } else {
        AsrMode::Cloud
    };
    let hardware = probe_hardware(&SystemCommandRunner);
    let mut secrets = SecretToolStore;
    let deepgram = secrets.diagnose(Provider::Deepgram);
    let groq = secrets.diagnose(Provider::Groq);
    let consented = production_consented_status();
    let choice = q.choose_engine(&EngineFacts {
        current,
        hardware: &hardware,
        deepgram: &deepgram,
        groq: &groq,
        interactive,
        consented: &consented,
    });
    // The engine screen skipped its own switch warning because the consent
    // prompt below confirms the switch; declining it must keep Cloud.
    let consent_confirms_switch = choice == EngineChoice::Switch(AsrMode::Local)
        && consent_confirms_local_switch(&hardware, &consented);
    match choice {
        EngineChoice::Keep(_) => {}
        EngineChoice::Switch(mode) => match crate::asr_mode::apply_cli_mode(mode) {
            Ok(message) => q.say(&message),
            Err(error) => return stop(q, &error.message),
        },
    }
    // Decided once: the optional Cloud key prompts below must not move a
    // Local setup onto the Cloud-only completion path.
    let mut local = crate::local_setup::is_local_mode();
    if local {
        // Consented Install offer first; the Bakeoff-catalog repair flow
        // after this stays evidence-gated and never offers a second download.
        let installed = matches!(consented, ConsentedStatus::Installed { .. });
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0);
        match q.install_local(&hardware, interactive, installed, now_unix) {
            ConsentedInstallStep::Failed => return SETUP_INCOMPLETE,
            ConsentedInstallStep::Declined if consent_confirms_switch => {
                if let Err(error) = crate::asr_mode::apply_cli_mode(AsrMode::Cloud) {
                    return stop(q, &error.message);
                }
                q.say("Local was not installed; keeping Cloud.");
                local = false;
            }
            step => {
                if let Err(error) = q.repair_local(step, installed) {
                    return stop(q, &error);
                }
            }
        }
    }
    if local {
        // A first-time Local user without Cloud keys is asked once; No keeps
        // `asr_mode = "local"`, skips every key prompt, and is remembered.
        let declined_marker = production_cloud_declined_marker();
        if q.cloud_after_local(interactive, &deepgram, &groq, declined_marker.as_deref()) {
            q.cloud_keys();
        }
        if wizard_only {
            q.say(&production_local_complete_message());
            return 0;
        }
    }
    let hyprland_config = discovery.hyprland_config;
    if hyprland_config.is_some()
        && !wizard_only
        && let Err(error) = crate::service::hyprland_overlay_preflight()
    {
        return stop(q, &error);
    }
    if local {
        if let Some(HyprlandConfig::CurrentLua(config)) = hyprland_config
            && let Err(error) = q.hyprland(&config)
        {
            return stop(q, &hyprland_incomplete(&error));
        }
        if discovery.profile == SetupProfile::UbuntuWayland {
            return finish_ubuntu(q, &production_local_complete_message());
        }
        q.say(&production_local_complete_message());
        return 0;
    }
    let outcome = q.cloud_keys();
    // A run that stored/kept no usable key (every provider skipped or its store
    // failed) did not accomplish setup's job, so it must not report success.
    let usable = |outcome: ProviderOutcome| {
        matches!(
            outcome,
            ProviderOutcome::Stored | ProviderOutcome::Kept | ProviderOutcome::StoredUnverified
        )
    };
    if !usable(outcome.deepgram) && !usable(outcome.groq) {
        return SETUP_INCOMPLETE;
    }
    if wizard_only {
        q.say(SETUP_COMPLETE_MESSAGE);
        return 0;
    }
    if let Some(HyprlandConfig::CurrentLua(config)) = hyprland_config {
        match q.hyprland(&config) {
            Ok(result) => {
                let paste = if result.paste_verified {
                    "your Hyprland paste shortcut is ready"
                } else {
                    "no paste shortcut found, so your words stay on the clipboard"
                };
                q.say(SETUP_COMPLETE_MESSAGE);
                q.say(&format!(
                    "Voisu is configured. Press {} to test; {paste}.",
                    result.trigger_key.label
                ));
                0
            }
            Err(error) => stop(q, &hyprland_incomplete(&error)),
        }
    } else if discovery.profile == SetupProfile::UbuntuWayland {
        finish_ubuntu(q, SETUP_COMPLETE_MESSAGE)
    } else {
        q.say(SETUP_COMPLETE_MESSAGE);
        0
    }
}

fn stop(q: &mut dyn SetupQuestions, message: &str) -> u8 {
    q.fail(message);
    SETUP_INCOMPLETE
}

fn hyprland_incomplete(error: &str) -> String {
    format!(
        "Hyprland setup incomplete: {error}; recovery: run `voisu setup` after fixing this step"
    )
}

fn finish_ubuntu(q: &mut dyn SetupQuestions, complete_message: &str) -> u8 {
    use crate::ubuntu_gnome::{LiveState, TriggerState};
    let result = match crate::ubuntu_gnome::setup() {
        Ok(result) => result,
        Err(error) => {
            return stop(
                q,
                &format!(
                    "Ubuntu setup incomplete: {error}; recovery: run `voisu setup` after fixing this step"
                ),
            );
        }
    };
    q.say(complete_message);
    q.say(match result.overlay {
        LiveState::Active => "Overlay: active (native GNOME capsule).",
        LiveState::RestartRequired => {
            "Overlay: enabled; restart required (log out and back in, or reboot)."
        }
        LiveState::Unavailable => "Overlay: unavailable.",
    });
    let key = result.trigger_key.as_deref().unwrap_or("configured");
    q.say(&match result.trigger {
        TriggerState::Active => format!("Trigger Key: {key}."),
        TriggerState::RestartRequired => {
            format!("Trigger Key: {key}; restart required before GNOME can install it.")
        }
        TriggerState::NotInstalled => {
            format!("Trigger Key: not installed; configured binding {key}; use `voisu toggle`.")
        }
    });
    q.say(if result.daemon_restart_required {
        "Daemon: enabled; restart required before it can start."
    } else {
        "Daemon: enabled and active."
    });
    0
}
