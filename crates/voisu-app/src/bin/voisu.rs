use std::io::{Read, Write};
use std::process::ExitCode;

use voisu_app::config::{DeliveryMode, RenderingPolicy, WritingMode};
use voisu_app::doctor::DoctorRow;
use voisu_app::service::{UserServiceAction, manage_user_service};
use voisu_app::system::{ProviderHttpClient, SecretToolStore};
use voisu_core::{
    AsrMode, BoundaryError, BoundaryFuture, BoundaryKind, Command, Credential, ExportCorrelationId,
    Provider, ProviderAuthenticator, ReadinessStatus, ReplayFixturePath, Response, SecretStore,
};

enum CliAction {
    Daemon(Command),
    History { json: bool },
    Replay { path: String, json: bool },
    Doctor { verbose: bool },
    Setup,
    AuthSet(Provider),
    AuthVerify(Provider),
    SetDeepgram(bool),
    SetAsrMode(AsrMode),
    Delivery(Option<DeliveryMode>),
    Writing(Option<WritingMode>),
    Rendering(Option<RenderingPolicy>),
    DictionaryAdd(String),
    DictionaryRemove(String),
    DictionaryList { json: bool },
    Service(UserServiceAction),
    LocalDev(Vec<String>),
    LocalRemove { yes: bool },
    Update(Vec<String>),
    Help,
    Version,
}

fn main() -> ExitCode {
    if voisu_app::tui::should_open() {
        return voisu_app::tui::run();
    }
    match parse_command() {
        Ok(CliAction::Daemon(command)) => daemon_command(command),
        Ok(CliAction::History { json }) => history_command(json),
        Ok(CliAction::Replay { path, json }) => replay_command(&path, json),
        Ok(CliAction::Doctor { verbose }) => doctor(verbose),
        Ok(CliAction::Setup) => setup(),
        Ok(CliAction::AuthSet(provider)) => match credential_from_stdin() {
            Ok(credential) => auth_set(provider, credential),
            Err(error) => fail(2, error.public_message()),
        },
        Ok(CliAction::AuthVerify(provider)) => auth_verify(provider),
        Ok(CliAction::SetDeepgram(enabled)) => set_deepgram(enabled),
        Ok(CliAction::SetAsrMode(mode)) => set_asr_mode(mode),
        Ok(CliAction::Delivery(mode)) => delivery(mode),
        Ok(CliAction::Writing(mode)) => writing(mode),
        Ok(CliAction::Rendering(policy)) => rendering(policy),
        Ok(CliAction::DictionaryAdd(term)) => dictionary_add(&term),
        Ok(CliAction::DictionaryRemove(term)) => dictionary_remove(&term),
        Ok(CliAction::DictionaryList { json }) => dictionary_list(json),
        Ok(CliAction::Service(action)) => match manage_user_service(action) {
            Ok(report) => {
                println!("{}", report.message);
                ExitCode::from(report.exit_code)
            }
            Err(message) => fail(4, &message),
        },
        Ok(CliAction::LocalDev(args)) => ExitCode::from(voisu_app::local_dev_cli::run(&args)),
        Ok(CliAction::LocalRemove { yes }) => local_remove(yes),
        Ok(CliAction::Update(args)) => ExitCode::from(voisu_app::update::run(&args)),
        Ok(CliAction::Help) => {
            println!("{}", usage());
            ExitCode::SUCCESS
        }
        Ok(CliAction::Version) => {
            println!("voisu {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Err(message) => fail(2, &message),
    }
}

/// Sends one command to the daemon and returns the typed response, or an
/// `ExitCode` (already reported to the user) when the round-trip fails.
fn send_command(command: Command) -> Result<Response, ExitCode> {
    use voisu_app::daemon_client::DaemonClientError;
    match voisu_app::daemon_client::send_command(command) {
        Ok(response) => Ok(response),
        Err(DaemonClientError::Socket(message)) => Err(fail(2, &message)),
        Err(DaemonClientError::Unavailable) => {
            println!("daemon unavailable");
            Err(ExitCode::from(3))
        }
        Err(DaemonClientError::Failed { code, message }) => Err(fail(code, &message)),
    }
}

fn daemon_command(command: Command) -> ExitCode {
    let response = match send_command(command) {
        Ok(response) => response,
        Err(code) => return code,
    };
    if response.ok {
        if let Some(export) = &response.export {
            match serde_json::to_string_pretty(export) {
                Ok(encoded) => println!("{encoded}"),
                Err(_) => return fail(1, "daemon returned an invalid diagnostic export"),
            }
        } else if let Some(history) = &response.history {
            // The complete bounded records — Source Transcripts, final
            // Transcript, timings, decision reasons — as structured JSON.
            match serde_json::to_string_pretty(history) {
                Ok(encoded) => println!("{encoded}"),
                Err(_) => return fail(1, "daemon returned an invalid diagnostic history"),
            }
        } else {
            voisu_app::asr_mode::write_cli_status(&response.message, response.asr_mode.as_ref());
        }
        ExitCode::SUCCESS
    } else {
        fail(4, &response.message)
    }
}

/// `voisu replay <fixture-name>`. Without `--json` the output is exactly the
/// historic human message. With `--json` the full machine-readable `Response`
/// is printed — evidence including the provider-tagged Source Transcripts, the
/// final Transcript text, the decision detail, and the provider timings — so
/// local evaluation can score a replay without scraping human output. The
/// response travels the same Unix-socket frame path as every command, so the
/// `MAX_RESPONSE_BYTES` read budget applies to it; the daemon additionally
/// clamps each evidence transcript to `MAX_STORED_TEXT`, keeping a replay
/// response orders of magnitude below that budget.
fn replay_command(path: &str, json: bool) -> ExitCode {
    let response = match send_command(Command::Replay(ReplayFixturePath::new(path))) {
        Ok(response) => response,
        Err(code) => return code,
    };
    if !response.ok {
        return fail(4, &response.message);
    }
    if json {
        return match serde_json::to_string_pretty(&response) {
            Ok(encoded) => {
                println!("{encoded}");
                ExitCode::SUCCESS
            }
            Err(_) => fail(1, "daemon returned an invalid replay response"),
        };
    }
    println!("{}", response.message);
    ExitCode::SUCCESS
}

/// `voisu history`. By default renders a human-first, newest-first view that
/// foregrounds tail latency and each Provider's outcome, paginating 20 at a time
/// when stdout and stdin are a TTY. `--json` prints the byte-for-byte raw
/// history response (the historic behavior) so scripts never break.
fn history_command(json: bool) -> ExitCode {
    let response = match send_command(Command::History) {
        Ok(response) => response,
        Err(code) => return code,
    };
    if !response.ok {
        return fail(4, &response.message);
    }
    let Some(history) = response.history else {
        println!("{}", response.message);
        return ExitCode::SUCCESS;
    };
    if json {
        // Byte-compatible with the historic `voisu history` output.
        return match serde_json::to_string_pretty(&history) {
            Ok(encoded) => {
                println!("{encoded}");
                ExitCode::SUCCESS
            }
            Err(_) => fail(1, "daemon returned an invalid diagnostic history"),
        };
    }
    let records = match serde_json::to_value(&history) {
        Ok(records) => records,
        Err(_) => return fail(1, "daemon returned an invalid diagnostic history"),
    };
    render_history_pretty(&records)
}

/// Prints the pretty history, paginating only when stdout AND stdin are a TTY.
/// Piped or scripted invocations print the first page without ever blocking.
fn render_history_pretty(records: &serde_json::Value) -> ExitCode {
    use std::io::IsTerminal;
    use voisu_app::history_view::{
        DEFAULT_PAGE_SIZE, RenderStyle, pagination_prompt, render_history_noninteractive,
        render_page,
    };

    let stdout = std::io::stdout();
    let color = stdout.is_terminal();
    let interactive = color && std::io::stdin().is_terminal();
    let style = RenderStyle {
        now_ms: voisu_core::unix_millis_now(),
        color,
        transcript_width: 72,
    };

    if !interactive {
        let out = render_history_noninteractive(records, DEFAULT_PAGE_SIZE, &style);
        print!("{out}");
        let _ = std::io::stdout().flush();
        return ExitCode::SUCCESS;
    }

    let mut start = 0;
    loop {
        let page = render_page(records, start, DEFAULT_PAGE_SIZE, &style);
        print!("{}", page.body);
        let _ = std::io::stdout().flush();
        start += page.shown;
        if page.remaining == 0 || page.shown == 0 {
            break;
        }
        print!("{}", pagination_prompt(page.remaining, DEFAULT_PAGE_SIZE));
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) => break, // EOF — stop paging.
            Ok(_) => {
                if line.trim_start().starts_with('q') {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    ExitCode::SUCCESS
}

fn doctor(verbose: bool) -> ExitCode {
    let rows = voisu_app::doctor::collect_rows();
    let has_failure = rows.iter().any(|row| row.status == ReadinessStatus::Fail);
    print_doctor_rows(&rows, verbose);
    if has_failure {
        ExitCode::from(4)
    } else {
        ExitCode::SUCCESS
    }
}

/// Column widths for the terse doctor table: `label  value  STATUS`. Fixed (not
/// computed) so the layout is deterministic regardless of which checks ran, and
/// followed by an explicit two-space gutter so a value that overflows its column
/// can never touch the status ("1.0.5 (WAV fallback)  PASS", never glued).
const DOCTOR_LABEL_WIDTH: usize = 17;
const DOCTOR_VALUE_WIDTH: usize = 20;

/// Print every doctor check as one terse line. A runnable action prints on its
/// own indented line only on FAIL; WARN and OFF keep to one line, with any
/// guidance behind --verbose.
fn print_doctor_rows(rows: &[DoctorRow], verbose: bool) {
    for row in rows {
        let value = row.value.as_deref().unwrap_or("");
        println!(
            "{:<label$}  {:<value$}  {}",
            row.label,
            value,
            row.status.cli_label(),
            label = DOCTOR_LABEL_WIDTH,
            value = DOCTOR_VALUE_WIDTH,
        );
        if row.status == ReadinessStatus::Fail
            && let Some(action) = &row.action
        {
            println!("    {action}");
        }
        if verbose {
            println!("    ({})", row.detail);
        }
    }
}

/// `voisu setup` — the interactive, re-runnable wizard that validates each key
/// live before storing it. All logic lives in the injectable `voisu_app::setup`
/// core; here we supply the real terminal, keyring, and live validator.
fn setup() -> ExitCode {
    use std::io::IsTerminal;
    use voisu_app::setup::{
        LiveHyprlandSetupActions, LiveKeyValidator, ProviderOutcome, SETUP_COMPLETE_MESSAGE,
        StdioWizard, run_consented_local_setup, run_hyprland_setup, run_setup,
    };
    use voisu_app::setup_consented::{
        ConsentedInstallStep, SystemCommandRunner, ask_cloud_after_local,
        developer_pilot_manifest_exists, probe_hardware, production_cloud_declined_marker,
        production_consented_status, production_install, production_restart_daemon,
        run_catalog_repair_if_needed, run_consented_install_step,
    };
    use voisu_app::setup_engine::{
        ConsentedStatus, EngineChoice, consent_confirms_local_switch, run_engine_screen,
    };
    use voisu_app::setup_profile::{
        HyprlandConfig, SetupProfile, discover_setup_profile, live_setup_facts,
    };

    // Integration tests can isolate the wizard while exercising the real CLI;
    // production never sets this seam.
    let wizard_only = std::env::var_os("VOISU_TEST_SETUP_WIZARD_ONLY").is_some();
    let mut wizard = StdioWizard;
    let discovery = match discover_setup_profile(&live_setup_facts()) {
        Ok(discovery) => discovery,
        Err(error) => return fail(4, &error.message()),
    };
    println!("Detected {} Setup Profile.", discovery.profile.as_str());
    let current = if voisu_app::local_setup::is_local_mode() {
        AsrMode::Local
    } else {
        AsrMode::Cloud
    };
    let hardware = probe_hardware(&SystemCommandRunner);
    let mut secrets = SecretToolStore;
    let deepgram = secrets.diagnose(Provider::Deepgram);
    let groq = secrets.diagnose(Provider::Groq);
    let consented = production_consented_status();
    let interactive = std::io::stdin().is_terminal();
    let choice = run_engine_screen(
        &mut wizard,
        current,
        &hardware,
        &deepgram,
        &groq,
        interactive,
        developer_pilot_manifest_exists,
        consented.clone(),
    );
    // The engine screen skipped its own switch warning because the consent
    // prompt below confirms the switch; declining it must keep Cloud.
    let consent_confirms_switch = choice == EngineChoice::Switch(AsrMode::Local)
        && consent_confirms_local_switch(&hardware, &consented);
    match choice {
        EngineChoice::Keep(_) => {}
        EngineChoice::Switch(mode) => match voisu_app::asr_mode::apply_cli_mode(mode) {
            Ok(message) => println!("{message}"),
            Err(error) => return fail(4, &error.message),
        },
    }
    // Decided once: the optional Cloud key prompts below must not move a
    // Local setup onto the Cloud-only completion path.
    let mut local = voisu_app::local_setup::is_local_mode();
    if local {
        // Consented Install offer first; the Bakeoff-catalog repair flow
        // after this stays evidence-gated and never offers a second download.
        let installed = matches!(consented, ConsentedStatus::Installed { .. });
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0);
        match run_consented_install_step(
            &mut wizard,
            &hardware,
            interactive,
            installed,
            now_unix,
            production_install,
            production_restart_daemon,
        ) {
            ConsentedInstallStep::Failed => return ExitCode::from(4),
            ConsentedInstallStep::Declined if consent_confirms_switch => {
                if let Err(error) = voisu_app::asr_mode::apply_cli_mode(AsrMode::Cloud) {
                    return fail(4, &error.message);
                }
                println!("Local was not installed; keeping Cloud.");
                local = false;
            }
            step => {
                if let Err(error) = run_catalog_repair_if_needed(
                    &mut wizard,
                    step,
                    installed,
                    run_consented_local_setup,
                ) {
                    return fail(4, &error);
                }
            }
        }
    }
    if local {
        // A first-time Local user without Cloud keys is asked once; No keeps
        // `asr_mode = "local"`, skips every key prompt, and is remembered.
        let declined_marker = production_cloud_declined_marker();
        if ask_cloud_after_local(
            &mut wizard,
            interactive,
            &deepgram,
            &groq,
            declined_marker.as_deref(),
        ) {
            run_setup(&mut wizard, &mut SecretToolStore, &mut LiveKeyValidator);
        }
        if wizard_only {
            println!("{}", local_complete_message());
            return ExitCode::SUCCESS;
        }
    }
    let hyprland_config = discovery.hyprland_config;
    if hyprland_config.is_some()
        && !wizard_only
        && let Err(error) = voisu_app::service::hyprland_overlay_preflight()
    {
        return fail(4, &error);
    }
    if local {
        if let Some(HyprlandConfig::CurrentLua(config)) = hyprland_config {
            let mut actions = LiveHyprlandSetupActions { io: &mut wizard };
            if let Err(error) = run_hyprland_setup(&config, &mut actions) {
                return fail(
                    4,
                    &format!(
                        "Hyprland setup incomplete: {error}; recovery: run `voisu setup` after fixing this step"
                    ),
                );
            }
        }
        if discovery.profile == SetupProfile::UbuntuWayland {
            return finish_ubuntu_setup(&local_complete_message());
        }
        println!("{}", local_complete_message());
        return ExitCode::SUCCESS;
    }
    let outcome = run_setup(&mut wizard, &mut SecretToolStore, &mut LiveKeyValidator);
    // A run that stored/kept no usable key (every provider skipped or its store
    // failed) did not accomplish setup's job, so it must not report success.
    let usable = |outcome: ProviderOutcome| {
        matches!(
            outcome,
            ProviderOutcome::Stored | ProviderOutcome::Kept | ProviderOutcome::StoredUnverified
        )
    };
    if usable(outcome.deepgram) || usable(outcome.groq) {
        if !wizard_only {
            if let Some(HyprlandConfig::CurrentLua(config)) = hyprland_config {
                let mut actions = LiveHyprlandSetupActions { io: &mut wizard };
                match run_hyprland_setup(&config, &mut actions) {
                    Ok(result) => {
                        let paste = if result.paste_verified {
                            "verified Paste Action"
                        } else {
                            "clipboard-only paste fallback"
                        };
                        println!("{SETUP_COMPLETE_MESSAGE}");
                        println!(
                            "Voisu is configured. Press {} to test; {paste}.",
                            result.trigger_key.label
                        );
                        ExitCode::SUCCESS
                    }
                    Err(error) => fail(
                        4,
                        &format!(
                            "Hyprland setup incomplete: {error}; recovery: run `voisu setup` after fixing this step"
                        ),
                    ),
                }
            } else if discovery.profile == SetupProfile::UbuntuWayland {
                finish_ubuntu_setup(SETUP_COMPLETE_MESSAGE)
            } else {
                println!("{SETUP_COMPLETE_MESSAGE}");
                ExitCode::SUCCESS
            }
        } else {
            println!("{SETUP_COMPLETE_MESSAGE}");
            ExitCode::SUCCESS
        }
    } else {
        ExitCode::from(4)
    }
}

fn local_complete_message() -> String {
    voisu_app::setup_consented::production_local_complete_message()
}

fn local_remove(yes: bool) -> ExitCode {
    use std::io::IsTerminal;
    use voisu_app::setup::StdioWizard;
    use voisu_app::setup_consented::{
        LocalCommand, LocalRemoveStatus, production_remove, run_local_remove,
    };

    let mut wizard = StdioWizard;
    let mode = if voisu_app::local_setup::is_local_mode() {
        AsrMode::Local
    } else {
        AsrMode::Cloud
    };
    match run_local_remove(
        &mut wizard,
        std::io::stdin().is_terminal(),
        LocalCommand::Remove { yes },
        mode,
        production_remove,
    ) {
        LocalRemoveStatus::Success => ExitCode::SUCCESS,
        LocalRemoveStatus::Usage(message) => fail(2, message),
        LocalRemoveStatus::Failed(message) => fail(4, &message),
    }
}

fn finish_ubuntu_setup(complete_message: &str) -> ExitCode {
    use voisu_app::ubuntu_gnome::{LiveState, TriggerState};
    match voisu_app::ubuntu_gnome::setup() {
        Ok(result) => {
            println!("{complete_message}");
            match result.overlay {
                LiveState::Active => println!("Overlay: active (native GNOME capsule)."),
                LiveState::RestartRequired => {
                    println!("Overlay: enabled; restart required (log out and back in, or reboot).")
                }
                LiveState::Unavailable => println!("Overlay: unavailable."),
            }
            match result.trigger {
                TriggerState::Active => println!(
                    "Trigger Key: {}.",
                    result.trigger_key.as_deref().unwrap_or("configured")
                ),
                TriggerState::RestartRequired => {
                    println!(
                        "Trigger Key: {}; restart required before GNOME can install it.",
                        result.trigger_key.as_deref().unwrap_or("configured")
                    )
                }
                TriggerState::NotInstalled => {
                    println!(
                        "Trigger Key: not installed; configured binding {}; use `voisu toggle`.",
                        result.trigger_key.as_deref().unwrap_or("configured")
                    )
                }
            }
            if result.daemon_restart_required {
                println!("Daemon: enabled; restart required before it can start.");
            } else {
                println!("Daemon: enabled and active.");
            }
            ExitCode::SUCCESS
        }
        Err(error) => fail(
            4,
            &format!(
                "Ubuntu setup incomplete: {error}; recovery: run `voisu setup` after fixing this step"
            ),
        ),
    }
}

fn auth_set(provider: Provider, credential: Credential) -> ExitCode {
    match SecretToolStore.replace(provider, credential) {
        Ok(()) => {
            println!("{} credential stored", provider.cli_label());
            ExitCode::SUCCESS
        }
        Err(error) => fail(4, error.public_message()),
    }
}

/// Persists the Deepgram on/off toggle to the local config file. The daemon
/// reads it at start, so the change takes effect on the next daemon start; the
/// message reminds the user to restart a running daemon.
fn set_asr_mode(mode: AsrMode) -> ExitCode {
    match voisu_app::asr_mode::apply_cli_mode(mode) {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(error) => fail(error.exit_code, &error.message),
    }
}

fn set_deepgram(enabled: bool) -> ExitCode {
    match voisu_app::config::set_deepgram_enabled(enabled) {
        Ok(_) => {
            println!(
                "Deepgram {} for new Recordings; restart the daemon to apply \
                 (voisu service restart)",
                if enabled { "enabled" } else { "disabled" }
            );
            ExitCode::SUCCESS
        }
        Err(message) => fail(4, &message),
    }
}

/// Reads or persists the Delivery mode. A running daemon resolves configuration
/// only at start, so writes apply after the next restart.
fn delivery(mode: Option<DeliveryMode>) -> ExitCode {
    let Some(mode) = mode else {
        println!(
            "delivery mode: {}",
            voisu_app::config::delivery_mode().as_str()
        );
        return ExitCode::SUCCESS;
    };
    match voisu_app::config::set_delivery_mode(mode) {
        Ok(_) => {
            println!(
                "Delivery mode set to {} for new Recordings; restart the daemon to apply \
                 (voisu service restart)",
                mode.as_str()
            );
            ExitCode::SUCCESS
        }
        Err(message) => fail(4, &message),
    }
}

/// Reads or persists the Writing Mode. Smart enables Intent Reconstruction;
/// Literal disables it. A running daemon resolves configuration only at start,
/// so writes apply after the next restart.
fn writing(mode: Option<WritingMode>) -> ExitCode {
    let Some(mode) = mode else {
        let current = voisu_app::config::writing_mode();
        println!(
            "writing mode: {} ({})",
            current.as_str(),
            intent_reconstruction_status(current)
        );
        return ExitCode::SUCCESS;
    };
    match voisu_app::config::set_writing_mode(mode) {
        Ok(_) => {
            println!(
                "Writing mode set to {} for new Recordings ({}); restart the daemon to apply \
                 (voisu service restart)",
                mode.as_str(),
                intent_reconstruction_status(mode)
            );
            ExitCode::SUCCESS
        }
        Err(message) => fail(4, &message),
    }
}

/// IR follows the persisted Writing Mode, not this CLI process's env. A
/// daemon unit `VOISU_ENABLE_INTENT_RECONSTRUCTION` force-off still wins
/// after restart; say so on Smart so a host drop-in is not hidden.
fn intent_reconstruction_status(mode: WritingMode) -> &'static str {
    match mode {
        WritingMode::Smart => "Intent Reconstruction on unless the daemon env forces it off",
        WritingMode::Literal => "Intent Reconstruction off",
    }
}

/// Reads or persists the Developer Prompt Rendering policy. A running daemon
/// resolves configuration only at start, so writes apply after the next restart.
fn rendering(policy: Option<RenderingPolicy>) -> ExitCode {
    let Some(policy) = policy else {
        println!(
            "rendering policy: {}",
            voisu_app::config::rendering_policy().as_str()
        );
        return ExitCode::SUCCESS;
    };
    match voisu_app::config::set_rendering_policy(policy) {
        Ok(_) => {
            println!(
                "Rendering policy set to {} for new Recordings; restart the daemon to apply \
                 (voisu service restart)",
                policy.as_str()
            );
            ExitCode::SUCCESS
        }
        Err(message) => fail(4, &message),
    }
}

fn dictionary_add(term: &str) -> ExitCode {
    match voisu_app::dictionary::add_user_term(term) {
        Ok(true) => {
            println!("dictionary term added: {term}");
            ExitCode::SUCCESS
        }
        Ok(false) => {
            println!("dictionary term already present: {term}");
            ExitCode::SUCCESS
        }
        Err(message) => fail(4, &message),
    }
}

fn dictionary_remove(term: &str) -> ExitCode {
    match voisu_app::dictionary::remove_user_term(term) {
        Ok(true) => {
            println!("dictionary term removed: {term}");
            ExitCode::SUCCESS
        }
        Ok(false) => fail(4, &format!("dictionary term not found: {term}")),
        Err(message) => fail(4, &message),
    }
}

fn dictionary_list(json: bool) -> ExitCode {
    match voisu_app::dictionary::user_terms() {
        Ok(terms) if json => match serde_json::to_string(&terms) {
            Ok(terms) => {
                println!("{terms}");
                ExitCode::SUCCESS
            }
            Err(_) => fail(1, "cannot encode dictionary terms as JSON"),
        },
        Ok(terms) => {
            for term in terms {
                println!("{term}");
            }
            ExitCode::SUCCESS
        }
        Err(message) => fail(4, &message),
    }
}

fn credential_from_stdin() -> Result<Credential, BoundaryError> {
    let mut credential = String::new();
    std::io::stdin()
        .read_to_string(&mut credential)
        .map_err(|_| {
            BoundaryError::new(
                BoundaryKind::SecretStorage,
                "cannot read credential from standard input",
            )
        })?;
    Credential::new(credential.trim_end().to_owned())
}

fn auth_verify(provider: Provider) -> ExitCode {
    eprintln!("{}", voisu_app::local_doctor::CLOUD_CREDENTIAL_MAINTENANCE);
    let credential = match SecretToolStore.load(provider) {
        Ok(credential) => credential,
        Err(error) => return fail(4, error.public_message()),
    };
    let mut authenticator = ProviderHttpClient;
    match block_on(ProviderAuthenticator::verify(
        &mut authenticator,
        provider,
        credential,
    )) {
        Ok(()) => {
            println!("{} authentication verified", provider.cli_label());
            ExitCode::SUCCESS
        }
        Err(error) => fail(4, error.public_message()),
    }
}

/// The app does not otherwise need an async runtime; auth adapters use the
/// shared async boundary shape to match Recording adapters, so run one future
/// to completion with a minimal current-thread Tokio runtime.
fn block_on<T>(future: BoundaryFuture<'_, T>) -> Result<T, BoundaryError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .map_err(|_| {
            BoundaryError::new(BoundaryKind::ProviderAuthentication, "runtime unavailable")
        })?
        .block_on(future)
}

fn parse_command() -> Result<CliAction, String> {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    match arguments.as_slice() {
        [command] if command == "start" => Ok(CliAction::Daemon(Command::Start)),
        [command] if command == "stop" => Ok(CliAction::Daemon(Command::Stop)),
        [command] if command == "toggle" => Ok(CliAction::Daemon(Command::Toggle)),
        [command] if command == "status" => Ok(CliAction::Daemon(Command::Status)),
        [command] if command == "shortcut" => Ok(CliAction::Daemon(Command::Shortcut)),
        [command] if command == "history" => Ok(CliAction::History { json: false }),
        [command, flag] if command == "history" && flag == "--json" => {
            Ok(CliAction::History { json: true })
        }
        [command, correlation_id] if command == "export" => Ok(CliAction::Daemon(Command::Export(
            ExportCorrelationId::new(correlation_id.clone()),
        ))),
        [command, path] if command == "replay" => Ok(CliAction::Replay {
            path: path.clone(),
            json: false,
        }),
        [command, flag, path] if command == "replay" && flag == "--json" => Ok(CliAction::Replay {
            path: path.clone(),
            json: true,
        }),
        [command, path, flag] if command == "replay" && flag == "--json" => Ok(CliAction::Replay {
            path: path.clone(),
            json: true,
        }),
        [command] if command == "doctor" => Ok(CliAction::Doctor { verbose: false }),
        [command, flag] if command == "doctor" && (flag == "--verbose" || flag == "-v") => {
            Ok(CliAction::Doctor { verbose: true })
        }
        [command] if command == "setup" => Ok(CliAction::Setup),
        [command] if command == "--help" || command == "-h" || command == "help" => {
            Ok(CliAction::Help)
        }
        [command] if command == "--version" || command == "-V" => Ok(CliAction::Version),
        [auth, set, provider] if auth == "auth" && set == "set" => {
            Ok(CliAction::AuthSet(parse_provider(provider)?))
        }
        [auth, verify, provider] if auth == "auth" && verify == "verify" => {
            Ok(CliAction::AuthVerify(parse_provider(provider)?))
        }
        [command, state] if command == "deepgram" => {
            Ok(CliAction::SetDeepgram(parse_toggle(state)?))
        }
        [command, mode] if command == "mode" => Ok(CliAction::SetAsrMode(parse_asr_mode(mode)?)),
        [command] if command == "delivery" => Ok(CliAction::Delivery(None)),
        [command, mode] if command == "delivery" => {
            Ok(CliAction::Delivery(Some(parse_delivery_mode(mode)?)))
        }
        [command] if command == "writing" => Ok(CliAction::Writing(None)),
        [command, mode] if command == "writing" => {
            Ok(CliAction::Writing(Some(parse_writing_mode(mode)?)))
        }
        [command] if command == "rendering" => Ok(CliAction::Rendering(None)),
        [command, policy] if command == "rendering" => {
            Ok(CliAction::Rendering(Some(parse_rendering_policy(policy)?)))
        }
        [dictionary, action, term] if dictionary == "dictionary" && action == "add" => {
            Ok(CliAction::DictionaryAdd(term.clone()))
        }
        [dictionary, action, term] if dictionary == "dictionary" && action == "remove" => {
            Ok(CliAction::DictionaryRemove(term.clone()))
        }
        [dictionary, action] if dictionary == "dictionary" && action == "list" => {
            Ok(CliAction::DictionaryList { json: false })
        }
        [dictionary, action, flag]
            if dictionary == "dictionary" && action == "list" && flag == "--json" =>
        {
            Ok(CliAction::DictionaryList { json: true })
        }
        [service, action] if service == "service" => {
            Ok(CliAction::Service(parse_service_action(action)?))
        }
        [command, rest @ ..] if command == "local" => {
            match voisu_app::setup_consented::parse_local_command(rest) {
                Ok(voisu_app::setup_consented::LocalCommand::Remove { yes }) => {
                    Ok(CliAction::LocalRemove { yes })
                }
                Err(message) => Err(message),
            }
        }
        [command, rest @ ..] if command == "local-dev" => Ok(CliAction::LocalDev(rest.to_vec())),
        [command, rest @ ..] if command == "update" => Ok(CliAction::Update(rest.to_vec())),
        _ => Err(usage().to_owned()),
    }
}

fn parse_service_action(value: &str) -> Result<UserServiceAction, String> {
    match value {
        "install" => Ok(UserServiceAction::Install),
        "start" => Ok(UserServiceAction::Start),
        "stop" => Ok(UserServiceAction::Stop),
        "restart" => Ok(UserServiceAction::Restart),
        "status" => Ok(UserServiceAction::Status),
        "uninstall" => Ok(UserServiceAction::Uninstall),
        _ => Err(
            "service action must be install, start, stop, restart, status, or uninstall".to_owned(),
        ),
    }
}

fn parse_asr_mode(value: &str) -> Result<AsrMode, String> {
    AsrMode::parse(value).ok_or_else(|| "mode must be local or cloud".to_owned())
}

fn parse_toggle(value: &str) -> Result<bool, String> {
    match value {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err("deepgram must be on or off".to_owned()),
    }
}

fn parse_delivery_mode(value: &str) -> Result<DeliveryMode, String> {
    match value {
        "paste" => Ok(DeliveryMode::Paste),
        "copy" => Ok(DeliveryMode::Copy),
        _ => Err("delivery must be paste or copy".to_owned()),
    }
}

fn parse_writing_mode(value: &str) -> Result<WritingMode, String> {
    match value {
        "smart" => Ok(WritingMode::Smart),
        "literal" => Ok(WritingMode::Literal),
        _ => Err("writing mode must be smart or literal".to_owned()),
    }
}

fn parse_rendering_policy(value: &str) -> Result<RenderingPolicy, String> {
    RenderingPolicy::parse(value)
        .ok_or_else(|| "rendering policy must be natural, adaptive, or structured".to_owned())
}

fn parse_provider(value: &str) -> Result<Provider, String> {
    match value {
        "groq" => Ok(Provider::Groq),
        "deepgram" => Ok(Provider::Deepgram),
        _ => Err("provider must be groq or deepgram".to_owned()),
    }
}

const USAGE: &str = "\
usage: voisu <command> [args]
       voisu           open the interface (on a real terminal)

commands:
  setup
  start
  stop
  toggle
  status
  shortcut
  history [--json]
  export <correlation-id>
  replay [--json] <fixture-name>
  doctor [--verbose|-v]
  auth set <groq|deepgram>
  auth verify <groq|deepgram>
  deepgram <on|off>
  mode <local|cloud>
  delivery [paste|copy]
  writing [smart|literal]
  rendering [natural|adaptive|structured]
  dictionary add <term>
  dictionary remove <term>
  dictionary list [--json]
  service <install|start|stop|restart|status|uninstall>
  local remove [--yes]
  local-dev register|select|status|replay
  update [--check|--dry-run] [--switch-to-bin]

options:
  -h, --help, help
  -V, --version";

fn usage() -> &'static str {
    USAGE
}

fn fail(code: u8, message: &str) -> ExitCode {
    eprintln!("{message}");
    ExitCode::from(code)
}
