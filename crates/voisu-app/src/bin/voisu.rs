use std::io::{Read, Write};
use std::process::ExitCode;

use voisu_app::config::{DeliveryMode, RenderingPolicy, StoredDelivery, Switch, Tone, WritingMode};
use voisu_app::doctor::DoctorRow;
use voisu_app::service::{UserServiceAction, manage_user_service};
use voisu_app::system::{ProviderHttpClient, SecretToolStore};
use voisu_core::{
    AsrMode, BoundaryError, BoundaryFuture, BoundaryKind, Command, Credential, ExportCorrelationId,
    Provider, ProviderAuthenticator, ReadinessStatus, ReplayFixturePath, Response, SecretStore,
};

enum CliAction {
    Daemon(Command),
    History { json: bool, verbose: bool },
    Replay { path: String, json: bool },
    Doctor { verbose: bool },
    Setup,
    KeySet(Provider),
    KeyCheck(Provider),
    SetDeepgram(bool),
    Switch(Switch, Option<bool>),
    Star,
    SetAsrMode(AsrMode),
    Delivery(Option<DeliveryMode>),
    Writing(Option<WritingMode>),
    Layout(Option<RenderingPolicy>),
    Tone(Option<Tone>),
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
        Ok(CliAction::History { json, verbose }) => history_command(json, verbose),
        Ok(CliAction::Replay { path, json }) => replay_command(&path, json),
        Ok(CliAction::Doctor { verbose }) => doctor(verbose),
        Ok(CliAction::Setup) => setup(),
        Ok(CliAction::KeySet(provider)) => match credential_from_stdin() {
            Ok(credential) => key_set(provider, credential),
            Err(error) => fail(2, error.public_message()),
        },
        Ok(CliAction::KeyCheck(provider)) => key_check(provider),
        Ok(CliAction::SetDeepgram(enabled)) => set_deepgram(enabled),
        Ok(CliAction::Switch(switch, state)) => switch_command(switch, state),
        Ok(CliAction::Star) => star(),
        Ok(CliAction::SetAsrMode(mode)) => set_asr_mode(mode),
        Ok(CliAction::Delivery(mode)) => delivery(mode),
        Ok(CliAction::Writing(mode)) => writing(mode),
        Ok(CliAction::Layout(policy)) => layout(policy),
        Ok(CliAction::Tone(tone)) => tone_command(tone),
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
    let status = matches!(command, Command::Status);
    let code = daemon_reply(command);
    // From the cache the TUI and `voisu update --check` keep; never the network.
    if status && let Some(line) = voisu_app::update_notice::cached_notice() {
        println!("{line}");
    }
    code
}

fn daemon_reply(command: Command) -> ExitCode {
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
fn history_command(json: bool, verbose: bool) -> ExitCode {
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
    render_history_pretty(&records, verbose)
}

/// Prints the pretty history, paginating only when stdout AND stdin are a TTY.
/// Piped or scripted invocations print the first page without ever blocking.
fn render_history_pretty(records: &serde_json::Value, verbose: bool) -> ExitCode {
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
        verbose,
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
/// live before storing it. The flow lives in `voisu_app::setup_flow`; here we
/// supply the text wizard.
fn setup() -> ExitCode {
    use std::io::IsTerminal;
    if voisu_app::tui::should_open_setup() {
        return voisu_app::tui::run_setup();
    }
    // Integration tests can isolate the wizard while exercising the real CLI;
    // production never sets this seam.
    let wizard_only = std::env::var_os("VOISU_TEST_SETUP_WIZARD_ONLY").is_some();
    ExitCode::from(voisu_app::setup_flow::run(
        &mut voisu_app::setup_flow::TextSetup,
        std::io::stdin().is_terminal(),
        wizard_only,
    ))
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

fn key_set(provider: Provider, credential: Credential) -> ExitCode {
    match SecretToolStore.replace(provider, credential) {
        Ok(()) => {
            println!("{} key saved", provider.cli_label());
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
                "Deepgram turned {}. Restart Voisu to use it: voisu service restart",
                if enabled { "on" } else { "off" }
            );
            ExitCode::SUCCESS
        }
        Err(message) => fail(4, &message),
    }
}

/// Reads or persists an On/Off setting. Each is read where it is used, so a
/// change applies without a restart.
fn switch_command(switch: Switch, state: Option<bool>) -> ExitCode {
    let word = |on: bool| if on { "on" } else { "off" };
    let Some(on) = state else {
        let on = voisu_app::config::switch(switch);
        println!(
            "{}: {} ({})",
            switch_label(switch),
            word(on),
            switch_meaning(switch, on)
        );
        return ExitCode::SUCCESS;
    };
    match voisu_app::config::set_switch(switch, on) {
        Ok(_) => {
            println!(
                "{} turned {} ({}).",
                switch_label(switch),
                word(on),
                switch_meaning(switch, on)
            );
            ExitCode::SUCCESS
        }
        Err(message) => fail(4, &message),
    }
}

fn switch_label(switch: Switch) -> &'static str {
    match switch {
        Switch::UpdateCheck => "Update check",
        Switch::StarPrompt => "Star prompt",
    }
}

fn switch_meaning(switch: Switch, on: bool) -> &'static str {
    match (switch, on) {
        (Switch::UpdateCheck, true) => {
            "once a day Voisu downloads a small version file from GitHub Releases"
        }
        (Switch::UpdateCheck, false) => {
            "Voisu never checks by itself; voisu update --check still does"
        }
        (Switch::StarPrompt, true) => "after a few days of use Voisu may ask for a GitHub star",
        (Switch::StarPrompt, false) => "Voisu never asks for a GitHub star",
    }
}

/// Opens the repository page and ends the star nudge for good. It works
/// whatever the nudge or the setting say. Without a browser it names the page.
fn star() -> ExitCode {
    use voisu_app::star_prompt;
    let path = star_prompt::state_path();
    match star_prompt::star(path.as_deref(), star_prompt::open_in_browser) {
        Ok(()) => println!("Opening {}. Thank you!", star_prompt::REPO_URL),
        Err(_) => println!(
            "Open {} in your browser to star Voisu. Thank you!",
            star_prompt::REPO_URL
        ),
    }
    ExitCode::SUCCESS
}

/// Reads or persists the Delivery mode. A running daemon resolves configuration
/// only at start, so writes apply after the next restart.
///
/// A legacy `clipboard` means different things in different sessions, and this
/// shell may not be in the service's session, so it reports what the value
/// does instead of resolving it here.
fn delivery(mode: Option<DeliveryMode>) -> ExitCode {
    let Some(mode) = mode else {
        if voisu_app::config::stored_delivery() == Some(StoredDelivery::LegacyClipboard) {
            println!(
                "Delivery: paste if Hyprland has a paste shortcut, otherwise copy (saved by an older Voisu). Choose one: voisu delivery paste or voisu delivery copy"
            );
        } else {
            println!("Delivery: {}", voisu_app::config::delivery_mode().as_str());
        }
        return ExitCode::SUCCESS;
    };
    match voisu_app::config::set_delivery_mode(mode) {
        Ok(_) => {
            println!(
                "Delivery set to {}. Restart Voisu to use it: voisu service restart",
                mode.as_str()
            );
            ExitCode::SUCCESS
        }
        Err(message) => fail(4, &message),
    }
}

/// Reads or persists the Writing Mode. Clean (Smart) enables Intent
/// Reconstruction; exact (Literal) disables it. A running daemon resolves this
/// only at start, so writes apply after the next restart.
fn writing(mode: Option<WritingMode>) -> ExitCode {
    let Some(mode) = mode else {
        let current = voisu_app::config::writing_mode();
        println!(
            "Writing: {} ({})",
            current.as_str(),
            writing_meaning(current)
        );
        return ExitCode::SUCCESS;
    };
    match voisu_app::config::set_writing_mode(mode) {
        Ok(_) => {
            println!(
                "Writing set to {} ({}). Restart Voisu to use it: voisu service restart",
                mode.as_str(),
                writing_meaning(mode)
            );
            ExitCode::SUCCESS
        }
        Err(message) => fail(4, &message),
    }
}

/// What each Writing Mode does, in the user's terms. Clean also lets Intent
/// Reconstruction fix misheard words when Groq and Deepgram disagree (unless a
/// daemon unit `VOISU_ENABLE_INTENT_RECONSTRUCTION` forces it off).
fn writing_meaning(mode: WritingMode) -> &'static str {
    match mode {
        WritingMode::Smart => "fixes punctuation, grammar, and misheard words",
        WritingMode::Literal => "your words as spoken",
    }
}

/// Reads or persists the Layout (the Rendering Policy). The daemon reads it at
/// each Recording start, so a change applies to the next Recording.
fn layout(policy: Option<RenderingPolicy>) -> ExitCode {
    let Some(policy) = policy else {
        let current = voisu_app::config::rendering_policy();
        println!(
            "Layout: {} ({})",
            voisu_app::config::layout_name(current),
            layout_meaning(current)
        );
        return ExitCode::SUCCESS;
    };
    match voisu_app::config::set_rendering_policy(policy) {
        Ok(_) => {
            println!(
                "Layout set to {} ({}). It applies to your next Recording.",
                voisu_app::config::layout_name(policy),
                layout_meaning(policy)
            );
            ExitCode::SUCCESS
        }
        Err(message) => fail(4, &message),
    }
}

fn layout_meaning(policy: RenderingPolicy) -> &'static str {
    match policy {
        RenderingPolicy::Natural => "punctuation only",
        RenderingPolicy::Adaptive => "organizes text when you speak in parts",
        RenderingPolicy::Structured => "adds section headings",
    }
}

/// Reads or persists the Tone. The daemon reads it at each Recording start,
/// so a change applies to the next Recording.
fn tone_command(tone: Option<Tone>) -> ExitCode {
    let Some(tone) = tone else {
        let current = voisu_app::config::tone();
        println!("Tone: {} ({})", current.as_str(), tone_meaning(current));
        return ExitCode::SUCCESS;
    };
    match voisu_app::config::set_tone(tone) {
        Ok(_) => {
            println!(
                "Tone set to {} ({}). It applies to your next Recording.",
                tone.as_str(),
                tone_meaning(tone)
            );
            ExitCode::SUCCESS
        }
        Err(message) => fail(4, &message),
    }
}

fn tone_meaning(tone: Tone) -> &'static str {
    match tone {
        Tone::Formal => "full words and a final full stop",
        Tone::Informal => "abbreviations like btw, no final full stop",
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

fn key_check(provider: Provider) -> ExitCode {
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
            println!("{} key works", provider.cli_label());
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
        [command] if command == "history" => Ok(CliAction::History {
            json: false,
            verbose: false,
        }),
        [command, flag] if command == "history" && flag == "--json" => Ok(CliAction::History {
            json: true,
            verbose: false,
        }),
        [command, flag] if command == "history" && (flag == "--verbose" || flag == "-v") => {
            Ok(CliAction::History {
                json: false,
                verbose: true,
            })
        }
        [command, recording_id] if command == "export" => Ok(CliAction::Daemon(Command::Export(
            ExportCorrelationId::new(recording_id.clone()),
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
        [key, set, provider] if key == "key" && set == "set" => {
            Ok(CliAction::KeySet(parse_provider(provider)?))
        }
        [key, check, provider] if key == "key" && check == "check" => {
            Ok(CliAction::KeyCheck(parse_provider(provider)?))
        }
        [command, state] if command == "deepgram" => {
            Ok(CliAction::SetDeepgram(parse_toggle(state)?))
        }
        [command] if command == "update-check" => Ok(CliAction::Switch(Switch::UpdateCheck, None)),
        [command, state] if command == "update-check" => Ok(CliAction::Switch(
            Switch::UpdateCheck,
            Some(parse_on_off(state, "update-check")?),
        )),
        [command] if command == "star" => Ok(CliAction::Star),
        [command] if command == "star-prompt" => Ok(CliAction::Switch(Switch::StarPrompt, None)),
        [command, state] if command == "star-prompt" => Ok(CliAction::Switch(
            Switch::StarPrompt,
            Some(parse_on_off(state, "star-prompt")?),
        )),
        [command, engine] if command == "engine" => {
            Ok(CliAction::SetAsrMode(parse_asr_mode(engine)?))
        }
        [command] if command == "delivery" => Ok(CliAction::Delivery(None)),
        [command, mode] if command == "delivery" => {
            Ok(CliAction::Delivery(Some(parse_delivery_mode(mode)?)))
        }
        [command] if command == "writing" => Ok(CliAction::Writing(None)),
        [command, mode] if command == "writing" => {
            Ok(CliAction::Writing(Some(parse_writing_mode(mode)?)))
        }
        [command] if command == "layout" => Ok(CliAction::Layout(None)),
        [command, layout] if command == "layout" => {
            Ok(CliAction::Layout(Some(parse_layout(layout)?)))
        }
        [command] if command == "tone" => Ok(CliAction::Tone(None)),
        [command, tone] if command == "tone" => Ok(CliAction::Tone(Some(parse_tone(tone)?))),
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
    AsrMode::parse(value).ok_or_else(|| "engine must be local or cloud".to_owned())
}

fn parse_toggle(value: &str) -> Result<bool, String> {
    match value {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err("deepgram must be on or off".to_owned()),
    }
}

fn parse_on_off(value: &str, command: &str) -> Result<bool, String> {
    match value {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err(format!("{command} must be on or off")),
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
        "clean" => Ok(WritingMode::Smart),
        "exact" => Ok(WritingMode::Literal),
        _ => Err("writing must be clean or exact".to_owned()),
    }
}

fn parse_layout(value: &str) -> Result<RenderingPolicy, String> {
    voisu_app::config::parse_layout(value)
        .ok_or_else(|| "layout must be plain, auto, or sections".to_owned())
}

fn parse_tone(value: &str) -> Result<Tone, String> {
    Tone::parse(value).ok_or_else(|| "tone must be formal or informal".to_owned())
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
       voisu                    open the interface (on a real terminal)

commands:
  setup                         set up Voisu step by step
  start                         start a Recording
  stop                          stop the Recording and deliver the text
  toggle                        start or stop a Recording
  status                        show what Voisu is doing
  shortcut                      show your Trigger Key
  history [--json|--verbose]    list recent Recordings
  export <recording-id>         print one Recording's details for a bug report
  doctor [--verbose|-v]         check that everything works
  key set <groq|deepgram>       save a key (reads it from standard input)
  key check <groq|deepgram>     check that a saved key works
  deepgram <on|off>             use Deepgram alongside Groq
  engine <local|cloud>          transcribe on this computer or online
  delivery [paste|copy]         paste the text for you, or only copy it
  writing [clean|exact]         fix grammar and misheard words, or keep your words as spoken
  layout [plain|auto|sections]  punctuation only, organize when you speak in parts, or add headings
  tone [formal|informal]        full words, or abbreviations like btw, no final full stop
  dictionary add <term>         add a word Voisu should spell your way
  dictionary remove <term>      remove a word
  dictionary list [--json]      list your words
  service <install|start|stop|restart|status|uninstall>
                                manage the Voisu service
  local remove [--yes]          delete the Local model
  update [--check|--dry-run] [--switch-to-bin]
                                update Voisu
  update-check [on|off]         check once a day for a newer version
  star                          star Voisu on GitHub (opens your browser)
  star-prompt [on|off]          let Voisu ask for a GitHub star

options:
  -h, --help, help              show this help
  -V, --version                 show the version";

fn usage() -> &'static str {
    USAGE
}

fn fail(code: u8, message: &str) -> ExitCode {
    eprintln!("{message}");
    ExitCode::from(code)
}
