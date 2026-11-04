//! Runs the one setup flow behind the setup screens. The flow keeps its own
//! thread, since it blocks on the network, on downloads and on the person; each
//! question goes to the screen as a [`SetupEvent`] and waits for a [`SetupAnswer`].
//!
//! A key the person enters arrives in a redacted [`SetupAnswer`], turns into a
//! [`Credential`] here, and never goes back: the screen hears only its length
//! and how the check went.

use std::cell::Cell;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread;
use std::time::{Duration, Instant};

use voisu_core::{AsrMode, Credential, KeyDiagnosis, KeyLocation, Provider, ProviderKeyStatus};
use voisu_core::{KeyDiagnosis as Diagnosis, SecretStore};

use crate::config;
use crate::hyprland_bindings::{
    RECOVERY_COMMAND, RIGHT_ALT, TriggerKey, TriggerOccupancy, VOISU_TOGGLE_COMMAND,
    VerifiedPasteAction,
};
use crate::local_model::{
    ConsentedCatalogEntry, ConsentedConsent, ConsentedError, HostHardware, InstallPhase,
    InstallProgress, suggest_local,
};
use crate::setup::{
    HyprlandSetupActions, HyprlandSetupOutcome, KeyValidator, LiveHyprlandSetupActions,
    LiveKeyValidator, ProviderOutcome, SETUP_COMPLETE_MESSAGE, SetupOutcome, WizardIo,
    install_live_trigger, run_hyprland_setup,
};
use crate::setup_consented::{
    ConsentedInstallStep, ask_cloud_after_local, consented_error_line, production_install_with,
    production_local_complete_message, production_restart_daemon,
};
use crate::setup_engine::{
    ConsentedStatus, EngineChoice, consent_confirms_local_switch, suggested_consented_entry,
};
use crate::setup_flow::{self, EngineFacts, SETUP_INCOMPLETE, SetupQuestions};
use crate::system::SecretToolStore;

use super::model::{PromptText, Reply, status_word};
use super::setup::{
    ChoiceRow, Choose, Finish, KeyStart, KeyState, ONE_MOMENT, STEPS, SetupAnswer, SetupEvent,
    Stage, Summary, SummaryRow, about_gb,
};
use super::worker::{load_shortcut, status_poll};

const STEP_ENGINE: u8 = 1;
const STEP_CLOUD_OR_LOCAL: u8 = 2;
const STEP_TRIGGER: u8 = 3;

/// A progress bar needs no more updates than a person can see.
const PROGRESS_EVERY: Duration = Duration::from_millis(100);
/// How long to wait on the desktop's own Trigger Key window.
const DESKTOP_WINDOW_WAIT: Duration = Duration::from_secs(300);
const DESKTOP_POLL: Duration = Duration::from_secs(1);
/// Voisu may still be starting when setup ends.
const RUNNING_TRIES: u32 = 10;
const RUNNING_RETRY: Duration = Duration::from_millis(300);

const DOWNLOAD_STOPPED: &str = "✕ The download stopped. Check your internet, then ↵ to try again.";
const ESC_USES_CLOUD: &str = "Esc uses Cloud for now.";
const ESC_LEAVES: &str = "Esc leaves setup.";
const REJECTED_NOTE: &str = "That key didn't work. Check you copied all of it.";
const COULD_NOT_TELL_NOTE: &str = "Voisu couldn't check it. ↵ saves it anyway.";
const SAVE_FAILED: &str = "Voisu couldn't save it. Check your keyring is unlocked, then try again.";
const NOTHING_TO_PASTE: &str = "Nothing to paste. Copy your key first.";
const NO_CLIPBOARD: &str = "Can't read the clipboard here. Use your terminal's paste key.";
const NO_WORKING_KEY: &str = "Voisu has no working key yet. Run setup again when you have one.";
const CAPS_NOTE: &str = "Caps Lock will no longer switch to capitals.";
const COMPOSE_NOTE: &str =
    "Caps Lock will no longer switch to capitals. Your Compose key on it will be removed.";
const CAPS_TAKEN_NOTE: &str =
    "Caps Lock is already used for something else, so Voisu leaves it alone.";
const TRIGGER_TOGGLES: &str = "press to start, again to stop";
const TRIGGER_HOLDS: &str = "hold while you talk";

/// Starts the flow. The returned sender takes the person's answers.
pub(super) fn spawn(replies: Sender<Reply>) -> Sender<SetupAnswer> {
    let (answer, answers) = channel();
    thread::spawn(move || {
        let mut questions = TuiQuestions::new(Link { replies, answers });
        let code = setup_flow::run(&mut questions, true, false);
        let (code, finish) = questions.finish(code);
        questions.link.send(SetupEvent::Finished { code, finish });
    });
    answer
}

/// The two directions between the flow and the screen.
struct Link {
    replies: Sender<Reply>,
    answers: Receiver<SetupAnswer>,
}

impl Link {
    fn send(&self, event: SetupEvent) {
        // The interface going away ends the process, and this thread with it.
        let _ = self.replies.send(Reply::Setup(event));
    }

    /// Asks one question. `None` when the person left, which every later
    /// question then answers the same way.
    fn ask(&self, left: &mut bool, choose: Choose) -> Option<usize> {
        if *left {
            return None;
        }
        self.send(SetupEvent::Ask(choose));
        loop {
            match self.answers.recv() {
                Ok(SetupAnswer::Picked(row)) => return Some(row),
                Ok(SetupAnswer::Left) | Err(_) => {
                    *left = true;
                    return None;
                }
                Ok(_) => {}
            }
        }
    }

    fn busy(&self, step: u8, line: &str) {
        self.send(SetupEvent::Busy {
            step,
            title: String::new(),
            line: line.to_owned(),
        });
    }
}

struct TuiQuestions {
    link: Link,
    left: bool,
    failure: Option<String>,
    /// Picking Local stands on the consent question alone, so declining that
    /// question goes back to Cloud.
    consent_confirms_switch: bool,
    /// Keys were asked for after Local, so none is fine.
    keys_optional: bool,
    keys: Option<(KeyEnd, KeyEnd)>,
    trigger: Option<TriggerKey>,
    deepgram_changed: bool,
}

impl TuiQuestions {
    fn new(link: Link) -> Self {
        Self {
            link,
            left: false,
            failure: None,
            consent_confirms_switch: false,
            keys_optional: false,
            keys: None,
            trigger: None,
            deepgram_changed: false,
        }
    }

    /// What the flow's exit code means for the person.
    fn finish(&mut self, code: u8) -> (u8, Finish) {
        if self.left {
            return (SETUP_INCOMPLETE, Finish::Left);
        }
        if code != 0 {
            let message = self.failure.take();
            return (
                code,
                Finish::Failed(message.unwrap_or_else(|| NO_WORKING_KEY.to_owned())),
            );
        }
        self.link.busy(STEPS, "Making sure Voisu is running…");
        if self.deepgram_changed {
            // The Deepgram setting is read at start. A failure here shows up as
            // "isn't running" just below.
            let _ = production_restart_daemon();
        }
        (code, Finish::Ready(self.summary()))
    }

    fn summary(&self) -> Summary {
        let local = crate::local_setup::is_local_mode();
        build_summary(&SummaryFacts {
            engine: if local {
                EngineEnd::Local {
                    ready: production_local_complete_message() == SETUP_COMPLETE_MESSAGE,
                }
            } else {
                let (groq, deepgram) = self.keys.unwrap_or((KeyEnd::Missing, KeyEnd::Missing));
                EngineEnd::Cloud { groq, deepgram }
            },
            trigger: self.trigger,
            shortcut: desktop_shortcut(),
            running: voisu_is_running(),
        })
    }
}

impl SetupQuestions for TuiQuestions {
    // Each screen already says what is happening.
    fn say(&mut self, _line: &str) {}

    fn fail(&mut self, message: &str) {
        self.failure = Some(message.to_owned());
    }

    fn left(&self) -> bool {
        self.left
    }

    fn choose_engine(&mut self, facts: &EngineFacts<'_>) -> EngineChoice {
        let current = facts.current;
        let keep = EngineChoice::Keep(current);
        let Some(row) = self.link.ask(&mut self.left, engine_question(current)) else {
            return keep;
        };
        let confirms = consent_confirms_local_switch(facts.hardware, facts.consented);
        let needs_no_confirm =
            confirms || matches!(facts.consented, ConsentedStatus::Installed { .. });
        let choice = match engine_choice(current, row == ROW_LOCAL, needs_no_confirm) {
            Some(choice) => choice,
            None => match self.link.ask(&mut self.left, local_warning_question()) {
                Some(ROW_USE_LOCAL_ANYWAY) => EngineChoice::Switch(AsrMode::Local),
                Some(_) => EngineChoice::Keep(AsrMode::Cloud),
                None => keep,
            },
        };
        self.consent_confirms_switch = confirms && choice == EngineChoice::Switch(AsrMode::Local);
        choice
    }

    fn install_local(
        &mut self,
        hardware: &HostHardware,
        interactive: bool,
        installed: bool,
        now_unix: u64,
    ) -> ConsentedInstallStep {
        if installed || !interactive {
            return ConsentedInstallStep::Skipped;
        }
        let Some(entry) = suggested_consented_entry(&suggest_local(hardware)) else {
            return ConsentedInstallStep::Skipped;
        };
        let question = consent_question(entry, self.consent_confirms_switch);
        // Consent exists only from here on, and only for this answer.
        if self.link.ask(&mut self.left, question) != Some(ROW_INSTALL) {
            return ConsentedInstallStep::Declined;
        }
        let (esc_says, esc) = if self.consent_confirms_switch {
            (ESC_USES_CLOUD, "use Cloud")
        } else {
            (ESC_LEAVES, "leave setup")
        };
        loop {
            self.link.send(SetupEvent::InstallStarted {
                step: STEP_CLOUD_OR_LOCAL,
                total: entry.total_download_bytes,
            });
            let replies = self.link.replies.clone();
            let consent = ConsentedConsent::accepted_entry(entry, now_unix);
            let error = match production_install_with(consent, move |_| {
                Box::new(ProgressToScreen::new(replies))
            }) {
                Ok(()) => {
                    // Failing here shows up as "isn't running" at the end.
                    let _ = production_restart_daemon();
                    return ConsentedInstallStep::Installed;
                }
                Err(error) => error,
            };
            self.link.send(SetupEvent::InstallFailed {
                message: install_failure(&error, esc_says),
                esc,
            });
            if !self.wait_for_retry() {
                if self.consent_confirms_switch {
                    return ConsentedInstallStep::Declined;
                }
                self.left = true;
                return ConsentedInstallStep::Failed;
            }
        }
    }

    // The text wizard's catalog repair is a developer path with its own prompts.
    // The Consented Install above is the one offer the screens make.
    fn repair_local(
        &mut self,
        _step: ConsentedInstallStep,
        _installed: bool,
    ) -> Result<(), String> {
        Ok(())
    }

    fn cloud_after_local(
        &mut self,
        interactive: bool,
        deepgram: &KeyDiagnosis,
        groq: &KeyDiagnosis,
        declined_marker: Option<&Path>,
    ) -> bool {
        let mut io = CloudQuestion {
            link: &self.link,
            left: &mut self.left,
        };
        let wanted = ask_cloud_after_local(&mut io, interactive, deepgram, groq, declined_marker);
        self.keys_optional = wanted;
        wanted
    }

    fn cloud_keys(&mut self) -> SetupOutcome {
        let result = run_keys(
            &self.link,
            &mut SecretToolStore,
            &mut LiveKeyValidator,
            &mut read_clipboard,
            !self.keys_optional,
        );
        self.left |= result.left;
        self.keys = Some((result.groq, result.deepgram));
        if !result.left {
            self.deepgram_changed |= apply_deepgram_choice(result.outcome.deepgram);
        }
        result.outcome
    }

    fn hyprland(&mut self, config: &Path) -> Result<HyprlandSetupOutcome, String> {
        self.link.busy(STEP_TRIGGER, ONE_MOMENT);
        let mut silent = SilentIo;
        let mut actions = TuiHyprland {
            live: LiveHyprlandSetupActions { io: &mut silent },
            link: &self.link,
            left: &mut self.left,
        };
        let outcome = run_hyprland_setup(config, &mut actions)?;
        self.trigger = Some(outcome.trigger_key);
        Ok(outcome)
    }

    fn desktop_trigger(&mut self) {
        if desktop_shortcut().is_some() {
            return;
        }
        if self.link.ask(&mut self.left, desktop_question()) != Some(ROW_OPEN_IT) {
            return;
        }
        // The daemon asks the desktop for its shortcut as it starts, which is
        // what opens the window.
        if production_restart_daemon().is_err() {
            return;
        }
        self.link.send(SetupEvent::Waiting {
            step: STEP_TRIGGER,
            title: DESKTOP_TITLE.to_owned(),
        });
        let deadline = Instant::now() + DESKTOP_WINDOW_WAIT;
        while Instant::now() < deadline && desktop_shortcut().is_none() {
            match self.link.answers.recv_timeout(DESKTOP_POLL) {
                Ok(SetupAnswer::Later | SetupAnswer::Left)
                | Err(RecvTimeoutError::Disconnected) => break,
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
            }
        }
        self.link.busy(STEP_TRIGGER, ONE_MOMENT);
    }
}

impl TuiQuestions {
    /// Whether the person wants another go after a failed install.
    fn wait_for_retry(&mut self) -> bool {
        loop {
            match self.link.answers.recv() {
                Ok(SetupAnswer::Retry) => return true,
                Ok(SetupAnswer::GiveUpInstall | SetupAnswer::Left) | Err(_) => return false,
                Ok(_) => {}
            }
        }
    }
}

const ROW_LOCAL: usize = 1;
const ROW_USE_LOCAL_ANYWAY: usize = 1;
const ROW_INSTALL: usize = 1;
const ROW_OPEN_IT: usize = 0;
const DESKTOP_TITLE: &str = "Choose a key that starts talking";

fn engine_question(current: AsrMode) -> Choose {
    Choose {
        step: STEP_ENGINE,
        title: "Where should your speech become text?".to_owned(),
        body: Vec::new(),
        rows: vec![
            ChoiceRow::new("Cloud", "Fast and accurate. Needs internet."),
            ChoiceRow::new("Local", "Private. Stays on this computer."),
        ],
        picked: usize::from(current == AsrMode::Local),
        note: Some("You can change this later in Settings.".to_owned()),
    }
}

/// The same decision as the text wizard's. `None` asks for a confirmation first.
fn engine_choice(
    current: AsrMode,
    wants_local: bool,
    needs_no_confirm: bool,
) -> Option<EngineChoice> {
    match (wants_local, current) {
        (false, AsrMode::Cloud) => Some(EngineChoice::Keep(AsrMode::Cloud)),
        (false, AsrMode::Local) => Some(EngineChoice::Switch(AsrMode::Cloud)),
        (true, AsrMode::Local) => Some(EngineChoice::Keep(AsrMode::Local)),
        (true, AsrMode::Cloud) if needs_no_confirm => Some(EngineChoice::Switch(AsrMode::Local)),
        (true, AsrMode::Cloud) => None,
    }
}

fn local_warning_question() -> Choose {
    Choose {
        step: STEP_ENGINE,
        title: "Local isn't installed".to_owned(),
        body: vec!["Voisu can't dictate with Local until it is.".to_owned()],
        rows: vec![
            ChoiceRow::new("Keep Cloud", "Fast and accurate. Needs internet."),
            ChoiceRow::new(
                "Use Local anyway",
                "Voisu can't dictate until it's installed.",
            ),
        ],
        picked: 0,
        note: None,
    }
}

fn consent_question(entry: &ConsentedCatalogEntry, keeps_cloud: bool) -> Choose {
    let not_now = if keeps_cloud {
        "Keep using Cloud."
    } else {
        "Voisu can't dictate until it's installed."
    };
    Choose {
        step: STEP_CLOUD_OR_LOCAL,
        title: "Install Local?".to_owned(),
        body: vec![format!(
            "Voisu downloads about {} GB once. Then your speech never leaves this computer.",
            about_gb(entry.total_download_bytes)
        )],
        rows: vec![
            ChoiceRow::new("Not now", not_now),
            ChoiceRow::new("Install", "Download and install now."),
        ],
        picked: 0,
        note: None,
    }
}

fn install_failure(error: &ConsentedError, esc_says: &str) -> String {
    match error {
        ConsentedError::Fetch(_) | ConsentedError::Deadline => {
            format!("{DOWNLOAD_STOPPED} {esc_says}")
        }
        other => format!(
            "✕ {} ↵ to try again. {esc_says}",
            consented_error_line(other)
        ),
    }
}

fn desktop_question() -> Choose {
    Choose {
        step: STEP_TRIGGER,
        title: DESKTOP_TITLE.to_owned(),
        body: vec![
            "Your desktop will open a small window.".to_owned(),
            "Press the key you want there, then save it.".to_owned(),
        ],
        rows: vec![
            ChoiceRow::new("Open it", ""),
            ChoiceRow::new("Later", "You can do this from Settings."),
        ],
        picked: ROW_OPEN_IT,
        note: None,
    }
}

/// The Trigger Key question, with whether its first row is Caps Lock. `None` when
/// Voisu already has a key on Hyprland.
fn trigger_question(occupancy: &TriggerOccupancy) -> Option<(Choose, bool)> {
    if occupancy.managed_key.is_some() {
        return None;
    }
    let caps = ChoiceRow::new("Caps Lock", "Press once to start, again to stop.");
    let right_alt = ChoiceRow::new("Right Alt", "Hold while you talk.");
    let (rows, note, offers_caps) = match &occupancy.unmanaged_caps_lock {
        Some((_, command)) if command == VOISU_TOGGLE_COMMAND => {
            (vec![caps, right_alt], None, true)
        }
        Some(_) => (vec![right_alt], Some(CAPS_TAKEN_NOTE), false),
        None if occupancy.compose_caps => (vec![caps, right_alt], Some(COMPOSE_NOTE), true),
        None => (vec![caps, right_alt], Some(CAPS_NOTE), true),
    };
    Some((
        Choose {
            step: STEP_TRIGGER,
            title: "Which key starts talking?".to_owned(),
            body: Vec::new(),
            rows,
            picked: 0,
            note: note.map(str::to_owned),
        },
        offers_caps,
    ))
}

/// Answers "Also set up Cloud?" for the flow's own decision, which also remembers
/// a No. Leaving is not a No: the flow keeps asking for a real answer, gets
/// `leave` each time without troubling anyone, and remembers nothing.
struct CloudQuestion<'a> {
    link: &'a Link,
    left: &'a mut bool,
}

impl WizardIo for CloudQuestion<'_> {
    fn writeln(&mut self, _line: &str) {}

    fn prompt_line(&mut self, _prompt: &str) -> Option<String> {
        let answer = self.link.ask(
            self.left,
            Choose {
                step: STEP_CLOUD_OR_LOCAL,
                title: "Also set up Cloud?".to_owned(),
                body: vec!["For when you want Cloud instead of Local. It needs a key.".to_owned()],
                rows: vec![
                    ChoiceRow::new("Not now", "Local is all you need."),
                    ChoiceRow::new("Set it up", "Paste a key now."),
                ],
                picked: 0,
                note: None,
            },
        );
        Some(
            match answer {
                Some(1) => "y",
                Some(_) => "n",
                None => "leave",
            }
            .to_owned(),
        )
    }

    fn prompt_secret(&mut self, _prompt: &str) -> Option<String> {
        None
    }
}

/// For the parts of the Hyprland setup that say something but ask nothing.
struct SilentIo;

impl WizardIo for SilentIo {
    fn writeln(&mut self, _line: &str) {}

    fn prompt_line(&mut self, _prompt: &str) -> Option<String> {
        None
    }

    fn prompt_secret(&mut self, _prompt: &str) -> Option<String> {
        None
    }
}

/// The live Hyprland setup, except that the Trigger Key is asked on screen.
struct TuiHyprland<'a> {
    live: LiveHyprlandSetupActions<'a>,
    link: &'a Link,
    left: &'a mut bool,
}

impl HyprlandSetupActions for TuiHyprland<'_> {
    fn select_clipboard_delivery(&mut self) -> Result<(), String> {
        self.live.select_clipboard_delivery()
    }

    fn install_services(&mut self) -> Result<(), String> {
        self.live.install_services()
    }

    fn install_trigger_binding(&mut self, config: &Path) -> Result<TriggerKey, String> {
        let (link, left) = (self.link, &mut *self.left);
        install_live_trigger(config, &mut |occupancy| {
            let Some((question, offers_caps)) = trigger_question(occupancy) else {
                return Ok(false);
            };
            let Some(row) = link.ask(left, question) else {
                return Err(format!(
                    "No Trigger Key was installed. Recovery: rerun `{RECOVERY_COMMAND}` or bind `{VOISU_TOGGLE_COMMAND}` yourself."
                ));
            };
            link.busy(STEP_TRIGGER, "Getting Voisu ready…");
            Ok(offers_caps && row == 0)
        })
    }

    fn discover_paste_action(&mut self) -> Option<VerifiedPasteAction> {
        self.live.discover_paste_action()
    }

    fn start_services_and_query_daemon(&mut self) -> Result<(), String> {
        self.live.start_services_and_query_daemon()
    }

    fn verify_delivery_backend(&mut self) -> Result<(), String> {
        self.live.verify_delivery_backend()
    }
}

/// Reports an install to the screen. The install calls this on one thread, once
/// per write, so it thins them out.
struct ProgressToScreen {
    replies: Sender<Reply>,
    so_far: Cell<u64>,
    sent_at: Cell<Instant>,
}

impl ProgressToScreen {
    fn new(replies: Sender<Reply>) -> Self {
        Self {
            replies,
            so_far: Cell::new(0),
            sent_at: Cell::new(Instant::now()),
        }
    }

    fn send(&self, event: SetupEvent) {
        let _ = self.replies.send(Reply::Setup(event));
    }
}

impl InstallProgress for ProgressToScreen {
    fn downloaded(&self, bytes: u64) {
        let so_far = self.so_far.get().saturating_add(bytes);
        self.so_far.set(so_far);
        if self.sent_at.get().elapsed() >= PROGRESS_EVERY {
            self.sent_at.set(Instant::now());
            self.send(SetupEvent::Downloaded { so_far });
        }
    }

    fn phase(&self, phase: InstallPhase) {
        self.send(SetupEvent::Stage(match phase {
            InstallPhase::BuildingRuntime => Stage::GettingReady,
            InstallPhase::CheckingGpu | InstallPhase::CheckingCpu => Stage::Checking,
        }));
    }
}

/// How one key ended up, for the closing summary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum KeyEnd {
    Missing,
    Works,
    NotChecked,
    Rejected,
}

struct KeysResult {
    outcome: SetupOutcome,
    groq: KeyEnd,
    deepgram: KeyEnd,
    left: bool,
}

/// One key while the keys screen is open. Holds the credentials, so no Debug.
struct KeySlot {
    provider: Provider,
    /// What the store already had, kept for checking and for moving it off the
    /// plaintext file.
    stored: Option<Credential>,
    plaintext: bool,
    /// A key the check could not judge, in case the person saves it anyway.
    pending: Option<Credential>,
    outcome: ProviderOutcome,
    end: KeyEnd,
}

impl KeySlot {
    fn read(provider: Provider, store: &mut dyn SecretStore) -> (Self, KeyStart) {
        let mut slot = Self {
            provider,
            stored: None,
            plaintext: false,
            pending: None,
            outcome: ProviderOutcome::Skipped,
            end: KeyEnd::Missing,
        };
        let variable = provider.environment_variable();
        let (state, note) = match store.diagnose(provider) {
            Diagnosis::Found {
                location,
                credential,
            } => {
                slot.stored = Some(credential);
                slot.plaintext = location == KeyLocation::PlaintextFile;
                slot.outcome = ProviderOutcome::Kept;
                slot.end = KeyEnd::NotChecked;
                if location == KeyLocation::EnvOverride {
                    (
                        KeyState::Outside,
                        Some(format!("Voisu is using the key set in {variable}.")),
                    )
                } else {
                    (KeyState::Saved, None)
                }
            }
            Diagnosis::EnvOverrideInvalid => (
                KeyState::Blank,
                Some(format!(
                    "{variable} is set but isn't a usable key. Fix or unset it first."
                )),
            ),
            // A locked or missing keyring: storing a new key says how that went.
            _ => (KeyState::Blank, None),
        };
        let start = KeyStart {
            provider,
            state,
            note,
        };
        (slot, start)
    }

    /// Checks a key the person entered, and stores it if it works.
    fn check(
        &mut self,
        credential: Credential,
        dots: usize,
        link: &Link,
        store: &mut dyn SecretStore,
        validator: &mut dyn KeyValidator,
    ) {
        self.pending = None;
        self.update(link, KeyState::Checking, Some(dots), None);
        let status = validator.validate(self.provider, &credential);
        match status {
            ProviderKeyStatus::Valid => {
                self.store(
                    credential,
                    KeyState::Works,
                    ProviderOutcome::Stored,
                    KeyEnd::Works,
                    link,
                    store,
                );
            }
            ProviderKeyStatus::InvalidKey => self.update(
                link,
                KeyState::Rejected,
                None,
                Some(REJECTED_NOTE.to_owned()),
            ),
            transient => {
                self.pending = Some(credential);
                self.update(
                    link,
                    KeyState::Transient(status_word(transient).to_owned()),
                    None,
                    Some(COULD_NOT_TELL_NOTE.to_owned()),
                );
            }
        }
    }

    fn save_anyway(&mut self, link: &Link, store: &mut dyn SecretStore) {
        match self.pending.take() {
            Some(credential) => self.store(
                credential,
                KeyState::Unverified,
                ProviderOutcome::StoredUnverified,
                KeyEnd::NotChecked,
                link,
                store,
            ),
            None => self.update(link, KeyState::Blank, Some(0), None),
        }
    }

    fn store(
        &mut self,
        credential: Credential,
        state: KeyState,
        outcome: ProviderOutcome,
        end: KeyEnd,
        link: &Link,
        store: &mut dyn SecretStore,
    ) {
        match store.replace(self.provider, credential) {
            Ok(()) => {
                self.outcome = outcome;
                self.end = end;
                self.stored = None;
                self.plaintext = false;
                self.update(link, state, None, None);
            }
            // The store's own message speaks to developers.
            Err(_) => self.update(
                link,
                KeyState::Failed(SAVE_FAILED.to_owned()),
                None,
                Some(SAVE_FAILED.to_owned()),
            ),
        }
    }

    fn update(&self, link: &Link, state: KeyState, dots: Option<usize>, note: Option<String>) {
        link.send(SetupEvent::KeyUpdate {
            provider: self.provider,
            state,
            dots,
            note,
        });
    }
}

/// What the clipboard held.
pub(super) enum ClipboardRead {
    Text(String),
    Empty,
    Unavailable,
}

/// `Ctrl+V` in raw mode reaches Voisu as a key, so Voisu asks for the clipboard
/// itself. Terminal paste keys arrive as a bracketed paste instead.
fn read_clipboard() -> ClipboardRead {
    let output = Command::new("wl-paste")
        .args(["--no-newline", "--type", "text"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    match output {
        Ok(output) if output.status.success() => {
            ClipboardRead::Text(String::from_utf8_lossy(&output.stdout).into_owned())
        }
        Ok(_) => ClipboardRead::Empty,
        Err(_) => ClipboardRead::Unavailable,
    }
}

/// The keys screen, start to finish. Answers queue while a check runs, so nothing
/// the person does is lost; the screen itself shows that a check is running.
fn run_keys(
    link: &Link,
    store: &mut dyn SecretStore,
    validator: &mut dyn KeyValidator,
    clipboard: &mut dyn FnMut() -> ClipboardRead,
    required: bool,
) -> KeysResult {
    let (mut groq, groq_start) = KeySlot::read(Provider::Groq, store);
    let (mut deepgram, deepgram_start) = KeySlot::read(Provider::Deepgram, store);
    link.send(SetupEvent::Keys {
        step: STEP_CLOUD_OR_LOCAL,
        fields: vec![groq_start, deepgram_start],
        required,
    });
    for slot in [&mut groq, &mut deepgram] {
        let Some(credential) = &slot.stored else {
            continue;
        };
        let status = validator.validate(slot.provider, credential);
        slot.end = match status {
            ProviderKeyStatus::Valid => KeyEnd::Works,
            ProviderKeyStatus::InvalidKey => KeyEnd::Rejected,
            _ => KeyEnd::NotChecked,
        };
        link.send(SetupEvent::SavedWord {
            provider: slot.provider,
            word: status_word(status).to_owned(),
        });
    }
    let left = loop {
        let answer = link.answers.recv();
        let Ok(answer) = answer else {
            break true;
        };
        match answer {
            SetupAnswer::CheckKey { provider, text } => {
                let slot = slot_for(provider, &mut groq, &mut deepgram);
                check_entered(slot, &text, link, store, validator);
            }
            SetupAnswer::PasteClipboard { provider } => {
                let slot = slot_for(provider, &mut groq, &mut deepgram);
                match clipboard() {
                    ClipboardRead::Unavailable => key_note(link, provider, NO_CLIPBOARD),
                    ClipboardRead::Empty => key_note(link, provider, NOTHING_TO_PASTE),
                    ClipboardRead::Text(pasted) => {
                        let mut text = PromptText::default();
                        text.paste(&pasted);
                        if text.chars() == 0 {
                            key_note(link, provider, NOTHING_TO_PASTE);
                        } else {
                            check_entered(slot, &text, link, store, validator);
                        }
                    }
                }
            }
            SetupAnswer::SaveAnyway { provider } => {
                slot_for(provider, &mut groq, &mut deepgram).save_anyway(link, store);
            }
            SetupAnswer::KeysDone => break false,
            SetupAnswer::Left => break true,
            _ => {}
        }
    };
    for slot in [&mut groq, &mut deepgram] {
        // Keeping a key from the plaintext file moves it to the keyring when the
        // keyring is back; otherwise it stays where it was.
        if slot.plaintext
            && let Some(credential) = slot.stored.take()
        {
            let _ = store.replace(slot.provider, credential);
        }
    }
    KeysResult {
        outcome: SetupOutcome {
            deepgram: deepgram.outcome,
            groq: groq.outcome,
        },
        groq: groq.end,
        deepgram: deepgram.end,
        left,
    }
}

fn slot_for<'a>(
    provider: Provider,
    groq: &'a mut KeySlot,
    deepgram: &'a mut KeySlot,
) -> &'a mut KeySlot {
    match provider {
        Provider::Groq => groq,
        Provider::Deepgram => deepgram,
    }
}

fn check_entered(
    slot: &mut KeySlot,
    text: &PromptText,
    link: &Link,
    store: &mut dyn SecretStore,
    validator: &mut dyn KeyValidator,
) {
    match Credential::new(text.as_str().trim().to_owned()) {
        Ok(credential) => slot.check(credential, text.chars(), link, store, validator),
        Err(_) => slot.update(
            link,
            KeyState::Rejected,
            Some(text.chars()),
            Some(REJECTED_NOTE.to_owned()),
        ),
    }
}

fn key_note(link: &Link, provider: Provider, note: &str) {
    link.send(SetupEvent::KeyNote {
        provider,
        note: note.to_owned(),
    });
}

/// Skipping Deepgram turns it off, and storing a key turns it back on. Returns
/// whether that changed anything.
fn apply_deepgram_choice(outcome: ProviderOutcome) -> bool {
    let enable = match outcome {
        ProviderOutcome::Skipped => false,
        ProviderOutcome::Stored | ProviderOutcome::StoredUnverified => true,
        ProviderOutcome::Kept => return false,
    };
    if config::deepgram_enabled() == enable {
        return false;
    }
    // A config that cannot be written leaves Deepgram as it was, which the
    // Settings tab shows and can change.
    config::set_deepgram_enabled(enable).is_ok()
}

fn desktop_shortcut() -> Option<String> {
    load_shortcut().filter(|shortcut| shortcut != "Not bound" && shortcut != "Unknown")
}

fn voisu_is_running() -> bool {
    for attempt in 0..RUNNING_TRIES {
        if attempt > 0 {
            thread::sleep(RUNNING_RETRY);
        }
        if matches!(status_poll(), super::model::StatusPoll::Up(_)) {
            return true;
        }
    }
    false
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EngineEnd {
    Local { ready: bool },
    Cloud { groq: KeyEnd, deepgram: KeyEnd },
}

struct SummaryFacts {
    engine: EngineEnd,
    /// The key setup just bound, on Hyprland.
    trigger: Option<TriggerKey>,
    /// The key the desktop reports, elsewhere.
    shortcut: Option<String>,
    running: bool,
}

fn row(ok: bool, label: &str, value: &str) -> SummaryRow {
    SummaryRow {
        ok,
        label: label.to_owned(),
        value: value.to_owned(),
    }
}

fn engine_row(engine: EngineEnd) -> SummaryRow {
    match engine {
        EngineEnd::Local { ready: true } => row(true, "Local", "On this computer"),
        EngineEnd::Local { ready: false } => row(
            false,
            "Local",
            "Not installed yet. Voisu can't dictate until it is.",
        ),
        EngineEnd::Cloud { groq, deepgram } => cloud_row(groq, deepgram),
    }
}

fn cloud_row(groq: KeyEnd, deepgram: KeyEnd) -> SummaryRow {
    use KeyEnd::{Rejected, Works};
    match (groq, deepgram) {
        (Rejected, _) => row(
            false,
            "Cloud",
            "Groq key was rejected. Replace it in Settings.",
        ),
        (_, Rejected) => row(
            false,
            "Cloud",
            "Deepgram key was rejected. Replace it in Settings.",
        ),
        (Works, Works) => row(true, "Cloud", "Groq and Deepgram keys work"),
        (Works, _) => row(true, "Cloud", "Groq key works"),
        (_, Works) => row(true, "Cloud", "Deepgram key works"),
        _ => row(true, "Cloud", "Keys saved, not checked yet"),
    }
}

fn build_summary(facts: &SummaryFacts) -> Summary {
    let mut rows = vec![engine_row(facts.engine)];
    let mut try_it = None;
    match (facts.trigger, &facts.shortcut) {
        (Some(key), _) => {
            let hold = key == RIGHT_ALT;
            let how = if hold { TRIGGER_HOLDS } else { TRIGGER_TOGGLES };
            rows.push(row(true, "Trigger Key", &format!("{} — {how}", key.label)));
            let label = key.label;
            try_it = Some(if hold {
                format!("Try it: click into any text box, hold {label}, speak, let go.")
            } else {
                format!("Try it: click into any text box, press {label}, speak, press it again.")
            });
        }
        (None, Some(shortcut)) => {
            rows.push(row(true, "Trigger Key", shortcut));
            try_it = Some(format!(
                "Try it: click into any text box, press {shortcut}, speak, press it again."
            ));
        }
        (None, None) => rows.push(row(
            false,
            "Trigger Key",
            "Not set yet. You can do this from Settings.",
        )),
    }
    rows.push(if facts.running {
        row(true, "Voisu is running", "")
    } else {
        row(false, "Voisu isn't running yet", "")
    });
    Summary { rows, try_it }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use voisu_core::{BoundaryError, BoundaryKind};

    use super::*;
    use crate::hyprland_bindings::CAPS_LOCK;
    use crate::local_model::{CONSENTED_ENTRY, PARAKEET_ENTRY};

    const KEY: &str = "gsk_secret_value";

    #[derive(Default)]
    struct FakeStore {
        keys: HashMap<&'static str, String>,
        plaintext: bool,
        env: bool,
        fail_replace: bool,
    }

    impl SecretStore for FakeStore {
        fn replace(
            &mut self,
            provider: Provider,
            credential: Credential,
        ) -> Result<(), BoundaryError> {
            if self.fail_replace {
                return Err(BoundaryError::new(
                    BoundaryKind::SecretStorage,
                    "Your keyring is locked.",
                ));
            }
            self.keys.insert(
                provider.secret_service_value(),
                credential.expose_to_boundary().to_owned(),
            );
            self.plaintext = false;
            Ok(())
        }

        fn load(&mut self, provider: Provider) -> Result<Credential, BoundaryError> {
            self.keys
                .get(provider.secret_service_value())
                .map(|key| Credential::new(key.clone()).expect("fake key"))
                .ok_or_else(|| BoundaryError::new(BoundaryKind::SecretStorage, "absent"))
        }

        fn diagnose(&mut self, provider: Provider) -> KeyDiagnosis {
            match self.load(provider) {
                Ok(credential) => KeyDiagnosis::Found {
                    location: if self.env {
                        KeyLocation::EnvOverride
                    } else if self.plaintext {
                        KeyLocation::PlaintextFile
                    } else {
                        KeyLocation::Keyring
                    },
                    credential,
                },
                Err(_) => KeyDiagnosis::Absent,
            }
        }
    }

    struct FakeValidator(ProviderKeyStatus);

    impl KeyValidator for FakeValidator {
        fn validate(&mut self, _: Provider, _: &Credential) -> ProviderKeyStatus {
            self.0
        }
    }

    fn typed(text: &str) -> PromptText {
        let mut typed = PromptText::default();
        typed.paste(text);
        typed
    }

    fn check(text: &str) -> SetupAnswer {
        SetupAnswer::CheckKey {
            provider: Provider::Groq,
            text: typed(text),
        }
    }

    struct Run {
        result: KeysResult,
        events: Vec<SetupEvent>,
    }

    /// Runs the keys screen against answers queued up in advance.
    fn run_with(
        store: &mut FakeStore,
        status: ProviderKeyStatus,
        clipboard: fn() -> ClipboardRead,
        answers: Vec<SetupAnswer>,
    ) -> Run {
        let (reply_tx, reply_rx) = channel();
        let (answer_tx, answer_rx) = channel();
        for answer in answers {
            answer_tx.send(answer).expect("queue");
        }
        let link = Link {
            replies: reply_tx,
            answers: answer_rx,
        };
        let mut clipboard = clipboard;
        let result = run_keys(
            &link,
            store,
            &mut FakeValidator(status),
            &mut clipboard,
            true,
        );
        drop(link);
        let events = reply_rx
            .into_iter()
            .map(|reply| match reply {
                Reply::Setup(event) => event,
                other => panic!("{other:?}"),
            })
            .collect();
        Run { result, events }
    }

    fn no_clipboard() -> ClipboardRead {
        ClipboardRead::Unavailable
    }

    fn state_of(event: &SetupEvent) -> Option<&KeyState> {
        match event {
            SetupEvent::KeyUpdate { state, .. } => Some(state),
            _ => None,
        }
    }

    fn states(run: &Run) -> Vec<&KeyState> {
        run.events.iter().filter_map(state_of).collect()
    }

    #[test]
    fn a_key_that_works_is_stored_and_reported_with_only_its_length() {
        let mut store = FakeStore::default();
        let run = run_with(
            &mut store,
            ProviderKeyStatus::Valid,
            no_clipboard,
            vec![check(KEY), SetupAnswer::KeysDone],
        );
        assert_eq!(store.keys["groq"], KEY);
        assert_eq!(run.result.outcome.groq, ProviderOutcome::Stored);
        assert_eq!(run.result.outcome.deepgram, ProviderOutcome::Skipped);
        assert_eq!(run.result.groq, KeyEnd::Works);
        assert!(!run.result.left);
        assert_eq!(states(&run), [&KeyState::Checking, &KeyState::Works]);

        let debug = format!("{:?}", run.events);
        assert!(!debug.contains(KEY), "{debug}");
        assert!(
            debug.contains(&format!("dots: Some({})", KEY.len())),
            "{debug}"
        );
    }

    #[test]
    fn a_rejected_key_is_never_stored() {
        let mut store = FakeStore::default();
        let run = run_with(
            &mut store,
            ProviderKeyStatus::InvalidKey,
            no_clipboard,
            vec![check(KEY), SetupAnswer::Left],
        );
        assert!(store.keys.is_empty());
        assert_eq!(run.result.outcome.groq, ProviderOutcome::Skipped);
        assert!(run.result.left);
        assert_eq!(states(&run), [&KeyState::Checking, &KeyState::Rejected]);
    }

    #[test]
    fn a_check_that_cannot_tell_stores_only_when_asked_to() {
        let mut store = FakeStore::default();
        let run = run_with(
            &mut store,
            ProviderKeyStatus::Unreachable,
            no_clipboard,
            vec![check(KEY), SetupAnswer::KeysDone],
        );
        assert!(store.keys.is_empty());
        assert_eq!(
            states(&run)[1],
            &KeyState::Transient("Can't reach it".to_owned())
        );

        let mut store = FakeStore::default();
        let run = run_with(
            &mut store,
            ProviderKeyStatus::Unreachable,
            no_clipboard,
            vec![
                check(KEY),
                SetupAnswer::SaveAnyway {
                    provider: Provider::Groq,
                },
                SetupAnswer::KeysDone,
            ],
        );
        assert_eq!(store.keys["groq"], KEY);
        assert_eq!(run.result.outcome.groq, ProviderOutcome::StoredUnverified);
        assert_eq!(run.result.groq, KeyEnd::NotChecked);
    }

    #[test]
    fn a_stored_key_is_checked_and_kept_unless_replaced() {
        let mut store = FakeStore::default();
        store.keys.insert("groq", "old".to_owned());
        let run = run_with(
            &mut store,
            ProviderKeyStatus::InvalidKey,
            no_clipboard,
            vec![SetupAnswer::KeysDone],
        );
        assert_eq!(store.keys["groq"], "old");
        assert_eq!(run.result.outcome.groq, ProviderOutcome::Kept);
        assert_eq!(run.result.groq, KeyEnd::Rejected);
        assert!(run.events.contains(&SetupEvent::SavedWord {
            provider: Provider::Groq,
            word: "Rejected".to_owned(),
        }));
    }

    #[test]
    fn a_rejected_replacement_keeps_the_stored_key() {
        let mut store = FakeStore::default();
        store.keys.insert("groq", "old".to_owned());
        let run = run_with(
            &mut store,
            ProviderKeyStatus::InvalidKey,
            no_clipboard,
            vec![check(KEY), SetupAnswer::KeysDone],
        );
        assert_eq!(store.keys["groq"], "old");
        assert_eq!(run.result.outcome.groq, ProviderOutcome::Kept);
    }

    #[test]
    fn a_key_from_the_plaintext_file_moves_to_the_keyring_when_kept() {
        let mut store = FakeStore {
            plaintext: true,
            ..FakeStore::default()
        };
        store.keys.insert("groq", "old".to_owned());
        let _ = run_with(
            &mut store,
            ProviderKeyStatus::Valid,
            no_clipboard,
            vec![SetupAnswer::KeysDone],
        );
        assert!(!store.plaintext, "stored again, so it moved");
    }

    #[test]
    fn a_key_from_the_environment_is_named_and_kept() {
        let mut store = FakeStore {
            env: true,
            ..FakeStore::default()
        };
        store.keys.insert("groq", "from-env".to_owned());
        let run = run_with(
            &mut store,
            ProviderKeyStatus::Valid,
            no_clipboard,
            vec![SetupAnswer::KeysDone],
        );
        let SetupEvent::Keys { fields, .. } = &run.events[0] else {
            panic!("{:?}", run.events[0]);
        };
        assert_eq!(fields[0].state, KeyState::Outside);
        assert_eq!(
            fields[0].note.as_deref(),
            Some("Voisu is using the key set in VOISU_GROQ_API_KEY.")
        );
        assert_eq!(run.result.outcome.groq, ProviderOutcome::Kept);
    }

    #[test]
    fn storing_can_fail_and_says_why() {
        let mut store = FakeStore {
            fail_replace: true,
            ..FakeStore::default()
        };
        let run = run_with(
            &mut store,
            ProviderKeyStatus::Valid,
            no_clipboard,
            vec![check(KEY), SetupAnswer::Left],
        );
        assert_eq!(states(&run)[1], &KeyState::Failed(SAVE_FAILED.to_owned()));
        assert_eq!(run.result.outcome.groq, ProviderOutcome::Skipped);
    }

    #[test]
    fn ctrl_v_checks_the_clipboard_or_says_why_it_could_not() {
        let paste = || SetupAnswer::PasteClipboard {
            provider: Provider::Groq,
        };
        let mut store = FakeStore::default();
        let run = run_with(
            &mut store,
            ProviderKeyStatus::Valid,
            || ClipboardRead::Text(format!("  {KEY}\n")),
            vec![paste(), SetupAnswer::KeysDone],
        );
        assert_eq!(store.keys["groq"], KEY);
        assert!(!format!("{:?}", run.events).contains(KEY));

        for (clipboard, note) in [
            (no_clipboard as fn() -> ClipboardRead, NO_CLIPBOARD),
            (|| ClipboardRead::Empty, NOTHING_TO_PASTE),
            (|| ClipboardRead::Text("   ".to_owned()), NOTHING_TO_PASTE),
        ] {
            let run = run_with(
                &mut FakeStore::default(),
                ProviderKeyStatus::Valid,
                clipboard,
                vec![paste(), SetupAnswer::KeysDone],
            );
            assert!(run.events.contains(&SetupEvent::KeyNote {
                provider: Provider::Groq,
                note: note.to_owned(),
            }));
        }
    }

    #[test]
    fn the_engine_decision_matches_the_text_wizard() {
        use AsrMode::{Cloud, Local};
        assert_eq!(
            engine_choice(Cloud, false, false),
            Some(EngineChoice::Keep(Cloud))
        );
        assert_eq!(
            engine_choice(Local, false, false),
            Some(EngineChoice::Switch(Cloud))
        );
        assert_eq!(
            engine_choice(Local, true, false),
            Some(EngineChoice::Keep(Local))
        );
        assert_eq!(
            engine_choice(Cloud, true, true),
            Some(EngineChoice::Switch(Local))
        );
        assert_eq!(engine_choice(Cloud, true, false), None);
    }

    #[test]
    fn leaving_a_question_is_not_an_answer() {
        let (reply_tx, _reply_rx) = channel();
        let (answer_tx, answer_rx) = channel();
        answer_tx.send(SetupAnswer::Left).expect("queue");
        let link = Link {
            replies: reply_tx,
            answers: answer_rx,
        };
        let mut left = false;
        assert_eq!(link.ask(&mut left, engine_question(AsrMode::Cloud)), None);
        assert!(left);
        // Nothing queued: a second question would block if it were really asked.
        assert_eq!(link.ask(&mut left, engine_question(AsrMode::Cloud)), None);
    }

    #[test]
    fn leaving_the_cloud_question_is_not_remembered_as_a_no() {
        let (reply_tx, _reply_rx) = channel();
        let (answer_tx, answer_rx) = channel();
        answer_tx.send(SetupAnswer::Left).expect("queue");
        let link = Link {
            replies: reply_tx,
            answers: answer_rx,
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("declined");
        let mut left = false;
        let mut io = CloudQuestion {
            link: &link,
            left: &mut left,
        };
        let wanted = ask_cloud_after_local(
            &mut io,
            true,
            &KeyDiagnosis::Absent,
            &KeyDiagnosis::Absent,
            Some(&marker),
        );
        assert!(!wanted);
        assert!(left);
        assert!(!marker.exists());
    }

    #[test]
    fn a_no_to_the_cloud_question_is_remembered() {
        let (reply_tx, _reply_rx) = channel();
        let (answer_tx, answer_rx) = channel();
        answer_tx.send(SetupAnswer::Picked(0)).expect("queue");
        let link = Link {
            replies: reply_tx,
            answers: answer_rx,
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("declined");
        let mut left = false;
        let mut io = CloudQuestion {
            link: &link,
            left: &mut left,
        };
        assert!(!ask_cloud_after_local(
            &mut io,
            true,
            &KeyDiagnosis::Absent,
            &KeyDiagnosis::Absent,
            Some(&marker),
        ));
        assert!(marker.exists());
    }

    #[test]
    fn the_consent_line_states_each_downloads_real_size_and_defaults_to_not_now() {
        let distil = consent_question(&CONSENTED_ENTRY, true);
        assert_eq!(
            distil.body[0],
            "Voisu downloads about 3 GB once. Then your speech never leaves this computer."
        );
        assert_eq!(distil.rows[distil.picked].label, "Not now");
        assert_eq!(distil.rows[0].line, "Keep using Cloud.");

        let parakeet = consent_question(&PARAKEET_ENTRY, false);
        assert!(
            parakeet.body[0].contains("about 0.8 GB"),
            "{:?}",
            parakeet.body
        );
        assert_eq!(parakeet.rows[parakeet.picked].label, "Not now");
        assert_ne!(parakeet.rows[0].line, "Keep using Cloud.");
    }

    #[test]
    fn the_install_failure_says_what_to_do_in_plain_words() {
        assert_eq!(
            install_failure(&ConsentedError::Deadline, ESC_USES_CLOUD),
            "✕ The download stopped. Check your internet, then ↵ to try again. Esc uses Cloud for now."
        );
        assert_eq!(
            install_failure(&ConsentedError::Deadline, ESC_LEAVES),
            "✕ The download stopped. Check your internet, then ↵ to try again. Esc leaves setup."
        );
        assert_eq!(
            install_failure(&ConsentedError::NoSpace, ESC_USES_CLOUD),
            "✕ Not enough free disk space. ↵ to try again. Esc uses Cloud for now."
        );
    }

    fn occupancy(caps: Option<(&str, &str)>, compose: bool) -> TriggerOccupancy {
        TriggerOccupancy {
            managed_key: None,
            unmanaged_caps_lock: caps.map(|(a, b)| (a.to_owned(), b.to_owned())),
            compose_caps: compose,
        }
    }

    #[test]
    fn the_trigger_question_never_takes_a_caps_lock_binding_someone_else_owns() {
        let managed = TriggerOccupancy {
            managed_key: Some(CAPS_LOCK),
            ..occupancy(None, false)
        };
        assert!(trigger_question(&managed).is_none(), "nothing to ask");

        let (free, caps) = trigger_question(&occupancy(None, false)).expect("asks");
        assert!(caps);
        assert_eq!(free.rows[free.picked].label, "Caps Lock");
        assert_eq!(free.note.as_deref(), Some(CAPS_NOTE));

        let (compose, _) = trigger_question(&occupancy(None, true)).expect("asks");
        assert_eq!(compose.note.as_deref(), Some(COMPOSE_NOTE));

        let (ours, caps) =
            trigger_question(&occupancy(Some(("Dictate", VOISU_TOGGLE_COMMAND)), false))
                .expect("asks");
        assert!(caps);
        assert_eq!(ours.note, None);

        let (taken, caps) =
            trigger_question(&occupancy(Some(("Mine", "kitty")), false)).expect("asks");
        assert!(!caps);
        assert_eq!(taken.rows.len(), 1);
        assert_eq!(taken.rows[0].label, "Right Alt");
        assert_eq!(taken.note.as_deref(), Some(CAPS_TAKEN_NOTE));
    }

    #[test]
    fn the_summary_says_what_works_and_how_to_try_it() {
        let summary = build_summary(&SummaryFacts {
            engine: EngineEnd::Cloud {
                groq: KeyEnd::Works,
                deepgram: KeyEnd::Missing,
            },
            trigger: Some(CAPS_LOCK),
            shortcut: None,
            running: true,
        });
        assert_eq!(summary.rows[0], row(true, "Cloud", "Groq key works"));
        assert_eq!(
            summary.rows[1],
            row(
                true,
                "Trigger Key",
                "Caps Lock — press to start, again to stop"
            )
        );
        assert_eq!(summary.rows[2], row(true, "Voisu is running", ""));
        assert_eq!(
            summary.try_it.as_deref(),
            Some("Try it: click into any text box, press Caps Lock, speak, press it again.")
        );
    }

    #[test]
    fn the_summary_tells_a_right_alt_user_to_hold_it() {
        let summary = build_summary(&SummaryFacts {
            engine: EngineEnd::Local { ready: true },
            trigger: Some(RIGHT_ALT),
            shortcut: None,
            running: true,
        });
        assert_eq!(summary.rows[0], row(true, "Local", "On this computer"));
        assert_eq!(
            summary.try_it.as_deref(),
            Some("Try it: click into any text box, hold Right Alt, speak, let go.")
        );
    }

    #[test]
    fn the_summary_never_claims_what_is_not_so() {
        let summary = build_summary(&SummaryFacts {
            engine: EngineEnd::Local { ready: false },
            trigger: None,
            shortcut: None,
            running: false,
        });
        assert!(summary.rows.iter().all(|row| !row.ok), "{summary:?}");
        assert_eq!(summary.try_it, None);

        let rejected = cloud_row(KeyEnd::Rejected, KeyEnd::Works);
        assert!(!rejected.ok);
        assert!(rejected.value.starts_with("Groq key was rejected"));
        assert!(
            cloud_row(KeyEnd::NotChecked, KeyEnd::Missing)
                .value
                .contains("not checked")
        );
        assert_eq!(
            cloud_row(KeyEnd::Works, KeyEnd::Works).value,
            "Groq and Deepgram keys work"
        );
    }

    #[test]
    fn the_desktop_questions_key_comes_back_in_the_summary() {
        let summary = build_summary(&SummaryFacts {
            engine: EngineEnd::Local { ready: true },
            trigger: None,
            shortcut: Some("Meta+Space".to_owned()),
            running: true,
        });
        assert_eq!(summary.rows[1], row(true, "Trigger Key", "Meta+Space"));
        assert!(
            summary
                .try_it
                .as_deref()
                .is_some_and(|line| line.contains("Meta+Space"))
        );
    }
}
