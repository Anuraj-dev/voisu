//! The off-thread half: everything that can block runs here, so the render
//! thread never waits on a socket, a keyring or the network.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use voisu_core::{
    AsrMode, Command, Credential, DiagnosticRecord, KeyDiagnosis, KeyLocation, Provider,
    ProviderKeyStatus, SecretStore, Transcript,
};

use crate::config;
use crate::daemon_client::{DaemonClientError, send_command};
use crate::history_view::{self, RenderStyle};
use crate::service::{UserServiceAction, manage_user_service};
use crate::setup::{KeyValidator, LiveKeyValidator};
use crate::setup_engine::ConsentedStatus;
use crate::system::{ClipboardBoundary, SecretToolStore, WlClipboard};

use super::model::{
    DaemonSnapshot, HistoryItem, Job, Refresh, Reply, ServiceVerb, SettingsSnapshot, StatusPoll,
    ToastKind, local_switch_allowed, trigger_key_value,
};
use super::text;

const STATUS_INTERVAL: Duration = Duration::from_millis(500);
const RESTART_HINT: &str = "Restart Voisu to apply.";

/// The two job queues. Slow work has its own thread so a doctor run on a bad
/// network cannot delay a settings change.
pub struct Workers {
    fast: Sender<Job>,
    slow: Sender<Job>,
}

impl Workers {
    pub fn start(replies: Sender<Reply>) -> Self {
        let (fast, fast_jobs) = channel();
        let (slow, slow_jobs) = channel();
        spawn_lane(fast_jobs, replies.clone());
        spawn_lane(slow_jobs, replies.clone());
        thread::spawn(move || poll_status(&replies));
        Self { fast, slow }
    }

    pub fn submit(&self, job: Job) {
        let lane = if job.is_slow() {
            &self.slow
        } else {
            &self.fast
        };
        // A closed lane means the process is on its way out.
        let _ = lane.send(job);
    }
}

fn spawn_lane(jobs: Receiver<Job>, replies: Sender<Reply>) {
    thread::spawn(move || {
        while let Ok(job) = jobs.recv() {
            execute(job, &replies);
        }
    });
}

fn poll_status(replies: &Sender<Reply>) {
    loop {
        if replies.send(Reply::Status(status_poll())).is_err() {
            return;
        }
        thread::sleep(STATUS_INTERVAL);
    }
}

fn status_poll() -> StatusPoll {
    match send_command(Command::Status) {
        Ok(response) if response.ok => match response.state {
            Some(state) => StatusPoll::Up(DaemonSnapshot {
                state,
                asr: response.asr_mode,
            }),
            None => StatusPoll::Failed,
        },
        Ok(_) => StatusPoll::Failed,
        Err(DaemonClientError::Failed { .. }) => StatusPoll::Failed,
        Err(DaemonClientError::Unavailable | DaemonClientError::Socket(_)) => {
            StatusPoll::Unavailable
        }
    }
}

/// Runs one job and sends what it learned. Every path that can change state goes
/// through the same library call its CLI command uses.
pub fn execute(job: Job, replies: &Sender<Reply>) {
    let send = |reply| {
        let _ = replies.send(reply);
    };
    match job {
        Job::LoadSettings => send(Reply::Settings(load_settings())),
        Job::Doctor => {
            // Setup state is local and instant, while doctor may need the network.
            send(Reply::SetupNeeded(setup_needed()));
            send(Reply::Doctor(crate::doctor::collect_rows()));
        }
        Job::LoadHistory => send(Reply::History(load_history())),
        Job::LoadDictionary => send(Reply::Dictionary(crate::dictionary::user_terms())),
        Job::LoadShortcut => send(Reply::Shortcut(load_shortcut())),
        Job::SetDelivery(mode) => send(applied_setting(
            config::set_delivery_mode(mode),
            format!("Delivery set to {}.", text::capitalized(mode.as_str())),
        )),
        Job::SetWriting(mode) => send(applied_setting(
            config::set_writing_mode(mode),
            format!("Writing set to {}.", text::capitalized(mode.as_str())),
        )),
        Job::SetRendering(policy) => send(applied_setting(
            config::set_rendering_policy(policy),
            format!("Rendering set to {}.", text::capitalized(policy.as_str())),
        )),
        Job::SetDeepgram(enabled) => send(applied_setting(
            config::set_deepgram_enabled(enabled),
            format!("Deepgram turned {}.", if enabled { "on" } else { "off" }),
        )),
        Job::SetEngine { mode, local_ready } => send(set_engine(mode, local_ready)),
        Job::AddTerm(term) => send(dictionary_result(
            crate::dictionary::add_user_term(&term),
            format!("Added \"{term}\"."),
            format!("\"{term}\" is already in your dictionary."),
        )),
        Job::RemoveTerm(term) => send(dictionary_result(
            crate::dictionary::remove_user_term(&term),
            format!("Removed \"{term}\"."),
            format!("\"{term}\" wasn't in your dictionary."),
        )),
        Job::SaveKey {
            provider,
            credential,
        } => {
            let (kind, text) = save_key(
                provider,
                credential,
                &mut LiveKeyValidator,
                &mut SecretToolStore,
            );
            send(Reply::KeyChecked { kind, text });
        }
        Job::Copy(text) => send(copy(text)),
        Job::Service(verb) => send(run_service(verb)),
    }
}

fn load_settings() -> SettingsSnapshot {
    SettingsSnapshot {
        engine: if crate::local_setup::is_local_mode() {
            AsrMode::Local
        } else {
            AsrMode::Cloud
        },
        delivery: config::delivery_mode(),
        writing: config::writing_mode(),
        rendering: config::rendering_policy(),
        deepgram: config::deepgram_enabled(),
    }
}

/// Setup has not run when Cloud mode has no key stored for either Provider. A
/// locked or missing keyring is a different problem, and the doctor reports it.
fn setup_needed() -> bool {
    if crate::local_setup::is_local_mode() {
        return false;
    }
    let mut store = SecretToolStore;
    [Provider::Groq, Provider::Deepgram]
        .into_iter()
        .all(|provider| matches!(store.diagnose(provider), KeyDiagnosis::Absent))
}

fn client_error_message(error: DaemonClientError) -> String {
    match error {
        DaemonClientError::Unavailable => {
            "Voisu isn't running, so your History can't be read yet.".to_owned()
        }
        DaemonClientError::Socket(message) | DaemonClientError::Failed { message, .. } => message,
    }
}

fn load_history() -> Result<Vec<HistoryItem>, String> {
    let response = send_command(Command::History).map_err(client_error_message)?;
    if !response.ok {
        return Err(response.message);
    }
    let now_ms = wall_clock_ms();
    let mut items: Vec<HistoryItem> = response
        .history
        .unwrap_or_default()
        .iter()
        .map(|record| history_item(record, now_ms))
        .collect();
    items.sort_by_key(|item| std::cmp::Reverse(item.recorded_at_ms));
    Ok(items)
}

/// Reduces a diagnostic record to what the tab shows, using the same wording
/// `voisu history` uses. Everything is sanitized here, once, at the boundary.
pub fn history_item(record: &DiagnosticRecord, now_ms: u64) -> HistoryItem {
    let value = serde_json::to_value(record).unwrap_or_default();
    let transcript = record
        .final_transcript
        .as_deref()
        .map(text::clean_block)
        .filter(|transcript| !transcript.trim().is_empty());
    let failure = transcript.is_none().then(|| {
        let reason = history_view::failure_reason(&value)
            .unwrap_or_else(|| "Voisu didn't say why.".to_owned());
        format!("No Transcript. {}", text::clean_line(&reason))
    });
    HistoryItem {
        recorded_at_ms: record.recorded_at_unix_ms,
        transcript,
        failure,
        providers: text::clean_line(&history_view::render_providers(
            &value,
            &RenderStyle::plain(now_ms),
        )),
    }
}

/// `None` only when no daemon answered, so the row never sticks on `Checking…`
/// against a daemon that replied without a Trigger Key.
fn load_shortcut() -> Option<String> {
    match send_command(Command::Shortcut) {
        Ok(response) if response.ok => Some(trigger_key_value(&response.message)),
        Ok(_) | Err(DaemonClientError::Failed { .. }) => Some("Unknown".to_owned()),
        Err(DaemonClientError::Unavailable | DaemonClientError::Socket(_)) => None,
    }
}

fn applied_setting(result: Result<std::path::PathBuf, String>, done: String) -> Reply {
    match result {
        Ok(_) => Reply::Applied {
            kind: ToastKind::Success,
            text: format!("{done} {RESTART_HINT}"),
            refresh: &[Refresh::Settings, Refresh::Doctor],
        },
        Err(message) => Reply::Applied {
            kind: ToastKind::Failure,
            text: format!("Nothing changed. {message}"),
            refresh: &[Refresh::Settings],
        },
    }
}

fn set_engine(mode: AsrMode, local_ready: bool) -> Reply {
    if mode == AsrMode::Local
        && !local_switch_allowed(
            local_ready,
            consented_install(crate::setup_consented::production_consented_status()),
        )
    {
        return Reply::Applied {
            kind: ToastKind::Caution,
            text: "Local isn't ready yet. Choose Run setup to install it.".to_owned(),
            refresh: &[Refresh::Settings],
        };
    }
    match crate::asr_mode::apply_cli_mode(mode) {
        Ok(message) => Reply::Applied {
            kind: ToastKind::Success,
            text: engine_applied_text(mode, &message),
            refresh: &[Refresh::Settings, Refresh::Doctor],
        },
        Err(error) => Reply::Applied {
            kind: ToastKind::Failure,
            text: format!("Engine not changed. {}", error.message),
            refresh: &[Refresh::Settings],
        },
    }
}

fn consented_install(status: ConsentedStatus) -> bool {
    matches!(status, ConsentedStatus::Installed { .. })
}

/// An offline change waits for the next daemon start, and the words say so.
fn engine_applied_text(mode: AsrMode, message: &str) -> String {
    let name = text::capitalized(mode.as_str());
    if message.contains("next supported daemon start") {
        format!("Engine set to {name}. It applies when Voisu next starts.")
    } else {
        format!("Engine set to {name}.")
    }
}

fn dictionary_result(result: Result<bool, String>, changed: String, unchanged: String) -> Reply {
    let (kind, text) = match result {
        Ok(true) => (ToastKind::Success, changed),
        Ok(false) => (ToastKind::Caution, unchanged),
        Err(message) => (ToastKind::Failure, message),
    };
    Reply::Applied {
        kind,
        text,
        refresh: &[Refresh::Dictionary],
    }
}

/// Checks a key live and stores it only if the Provider accepts it, the same
/// order Setup uses. Library fallback notices go to a silenced stderr while the
/// interface is up, so where the key landed is reported here instead.
pub fn save_key(
    provider: Provider,
    credential: Credential,
    validator: &mut dyn KeyValidator,
    store: &mut dyn SecretStore,
) -> (ToastKind, String) {
    let name = text::capitalized(provider.cli_label());
    match validator.validate(provider, &credential) {
        ProviderKeyStatus::Valid => {}
        ProviderKeyStatus::InvalidKey => {
            return (
                ToastKind::Failure,
                format!("{name} rejected that key. Nothing was saved."),
            );
        }
        ProviderKeyStatus::RateLimited
        | ProviderKeyStatus::QuotaExhausted
        | ProviderKeyStatus::Unreachable => {
            return (
                ToastKind::Caution,
                format!("Couldn't check that key with {name} right now. Nothing was saved."),
            );
        }
    }
    if store.replace(provider, credential).is_err() {
        return (
            ToastKind::Failure,
            format!("{name} accepted that key, but it couldn't be stored. Try Run setup."),
        );
    }
    match store.diagnose(provider) {
        KeyDiagnosis::Found {
            location: KeyLocation::PlaintextFile,
            ..
        } => (
            ToastKind::Caution,
            format!("{name} key saved to a private file, because no keyring is available."),
        ),
        KeyDiagnosis::Found {
            location: KeyLocation::EnvOverride,
            ..
        }
        | KeyDiagnosis::EnvOverrideInvalid => (
            ToastKind::Caution,
            format!(
                "{name} key saved, but {} overrides it.",
                provider.environment_variable()
            ),
        ),
        _ => (ToastKind::Success, format!("{name} key saved.")),
    }
}

fn copy(transcript: String) -> Reply {
    let failed = |text: String| Reply::Applied {
        kind: ToastKind::Failure,
        text,
        refresh: &[],
    };
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return failed("Couldn't copy that Transcript.".to_owned());
    };
    let mut clipboard = WlClipboard;
    match runtime.block_on(clipboard.preserve(&Transcript(transcript))) {
        Ok(()) => Reply::Applied {
            kind: ToastKind::Success,
            text: "Transcript copied.".to_owned(),
            refresh: &[],
        },
        Err(error) => failed(format!("Couldn't copy. {}.", error.public_message())),
    }
}

fn run_service(verb: ServiceVerb) -> Reply {
    let (action, done, verb_word) = match verb {
        ServiceVerb::Start => (UserServiceAction::Start, "Voisu started.", "start"),
        ServiceVerb::Restart => (UserServiceAction::Restart, "Voisu restarted.", "restart"),
    };
    let (kind, text) = match manage_user_service(action) {
        Ok(report) if report.exit_code == 0 => (ToastKind::Success, done.to_owned()),
        Ok(report) => (
            ToastKind::Failure,
            format!("Couldn't {verb_word} Voisu. {}", report.message),
        ),
        Err(message) => (
            ToastKind::Failure,
            format!("Couldn't {verb_word} Voisu. {message}"),
        ),
    };
    Reply::ServiceDone { kind, text }
}

fn wall_clock_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use voisu_core::BoundaryError;

    struct Canned(ProviderKeyStatus);

    impl KeyValidator for Canned {
        fn validate(&mut self, _: Provider, _: &Credential) -> ProviderKeyStatus {
            self.0
        }
    }

    #[derive(Default)]
    struct Store {
        saved: Vec<Provider>,
        location: Option<KeyLocation>,
        refuse: bool,
    }

    impl SecretStore for Store {
        fn load(&mut self, _: Provider) -> Result<Credential, BoundaryError> {
            Credential::new("stored".to_owned())
        }

        fn replace(&mut self, provider: Provider, _: Credential) -> Result<(), BoundaryError> {
            if self.refuse {
                return Err(BoundaryError::new(
                    voisu_core::BoundaryKind::SecretStorage,
                    "refused",
                ));
            }
            self.saved.push(provider);
            Ok(())
        }

        fn diagnose(&mut self, _: Provider) -> KeyDiagnosis {
            KeyDiagnosis::Found {
                location: self.location.unwrap_or(KeyLocation::Keyring),
                credential: Credential::new("stored".to_owned()).expect("valid"),
            }
        }
    }

    fn key() -> Credential {
        Credential::new("gsk_secret".to_owned()).expect("valid")
    }

    #[test]
    fn a_valid_key_is_stored() {
        let mut store = Store::default();
        let (kind, text) = save_key(
            Provider::Groq,
            key(),
            &mut Canned(ProviderKeyStatus::Valid),
            &mut store,
        );
        assert_eq!(kind, ToastKind::Success);
        assert_eq!(store.saved, [Provider::Groq]);
        assert!(!text.contains("gsk_secret"));
    }

    #[test]
    fn a_rejected_or_unverifiable_key_is_never_stored() {
        for status in [
            ProviderKeyStatus::InvalidKey,
            ProviderKeyStatus::RateLimited,
            ProviderKeyStatus::QuotaExhausted,
            ProviderKeyStatus::Unreachable,
        ] {
            let mut store = Store::default();
            let (_, text) = save_key(Provider::Deepgram, key(), &mut Canned(status), &mut store);
            assert!(store.saved.is_empty(), "{status:?} stored the key");
            assert!(text.contains("Nothing was saved"), "{text}");
            assert!(!text.contains("gsk_secret"));
        }
    }

    #[test]
    fn a_key_outside_the_keyring_is_called_out() {
        let mut store = Store {
            location: Some(KeyLocation::PlaintextFile),
            ..Store::default()
        };
        let (kind, text) = save_key(
            Provider::Groq,
            key(),
            &mut Canned(ProviderKeyStatus::Valid),
            &mut store,
        );
        assert_eq!(kind, ToastKind::Caution);
        assert!(text.contains("private file"), "{text}");
    }

    #[test]
    fn a_store_failure_is_reported_and_the_key_stays_out_of_the_words() {
        let mut store = Store {
            refuse: true,
            ..Store::default()
        };
        let (kind, text) = save_key(
            Provider::Groq,
            key(),
            &mut Canned(ProviderKeyStatus::Valid),
            &mut store,
        );
        assert_eq!(kind, ToastKind::Failure);
        assert!(!text.contains("gsk_secret"));
    }

    #[test]
    fn engine_toast_says_when_the_change_waits_for_a_restart() {
        assert_eq!(
            engine_applied_text(
                AsrMode::Cloud,
                "ASR mode set to cloud; it applies at the next supported daemon start"
            ),
            "Engine set to Cloud. It applies when Voisu next starts."
        );
        assert_eq!(
            engine_applied_text(AsrMode::Local, "ASR mode set to local"),
            "Engine set to Local."
        );
    }

    #[test]
    fn only_an_installed_consented_model_counts_as_consent() {
        assert!(consented_install(ConsentedStatus::Installed {
            entry_id: "x".to_owned(),
            device: "cpu".to_owned(),
        }));
        assert!(!consented_install(ConsentedStatus::NotInstalled));
        assert!(!consented_install(ConsentedStatus::Unknown("x".to_owned())));
    }
}
