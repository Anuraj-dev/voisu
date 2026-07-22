use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use voisu_core::{
    BoundaryError, BoundaryFuture, BoundaryKind, Command, Credential, ExportCorrelationId,
    KeyDiagnosis, KeyLocation, PROTOCOL_VERSION, Provider, ProviderAuthenticator, ProviderKeyStatus,
    ReadinessInspector, ReadinessStatus, ReplayFixturePath, Request, Response, SecretStore,
    VersionEnvelope, provider_free_tier_hint, socket_path,
};
use voisu_app::service::{UserServiceAction, manage_user_service};
use voisu_app::system::{
    FedoraReadiness, PROCESSING_RESPONSE_DEADLINE, ProviderHttpClient, SecretToolStore,
};
use voisu_app::config::DeliveryMode;

// History and export responses carry bounded local diagnostics, so the CLI
// accepts a larger response frame than the tiny command replies. The bound
// comfortably covers the full retained history: bounded record count times the
// clamped transcript sizes.
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;
const IO_DEADLINE: Duration = Duration::from_secs(2);

enum CliAction {
    Daemon(Command),
    History { json: bool },
    Doctor { verbose: bool },
    Setup,
    AuthSet(Provider),
    AuthVerify(Provider),
    SetDeepgram(bool),
    Delivery(Option<DeliveryMode>),
    DictionaryAdd(String),
    DictionaryRemove(String),
    DictionaryList { json: bool },
    Service(UserServiceAction),
    Help,
    Version,
}

fn main() -> ExitCode {
    match parse_command() {
        Ok(CliAction::Daemon(command)) => daemon_command(command),
        Ok(CliAction::History { json }) => history_command(json),
        Ok(CliAction::Doctor { verbose }) => doctor(verbose),
        Ok(CliAction::Setup) => setup(),
        Ok(CliAction::AuthSet(provider)) => match credential_from_stdin() {
            Ok(credential) => auth_set(provider, credential),
            Err(error) => fail(2, error.public_message()),
        },
        Ok(CliAction::AuthVerify(provider)) => auth_verify(provider),
        Ok(CliAction::SetDeepgram(enabled)) => set_deepgram(enabled),
        Ok(CliAction::Delivery(mode)) => delivery(mode),
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
    let path = match socket_path() {
        Ok(path) => path,
        Err(message) => return Err(fail(2, &message)),
    };
    let mut stream = match UnixStream::connect(path) {
        Ok(stream) => stream,
        Err(_) => {
            println!("daemon unavailable");
            return Err(ExitCode::from(3));
        }
    };

    if stream.set_write_timeout(Some(IO_DEADLINE)).is_err() {
        return Err(fail(1, "failed to configure daemon connection deadline"));
    }

    // A replay drives the same provider/validation boundaries as Stop, so it
    // shares the longer processing budget.
    let response_deadline = if matches!(
        command,
        Command::Stop | Command::Toggle | Command::Replay(_)
    ) {
        PROCESSING_RESPONSE_DEADLINE
    } else {
        IO_DEADLINE
    };
    let request = Request {
        version: PROTOCOL_VERSION,
        command,
    };
    if serde_json::to_writer(&mut stream, &request).is_err() || stream.write_all(b"\n").is_err() {
        return Err(fail(1, "failed to send command to daemon"));
    }
    let response = match read_response_frame(&mut stream, response_deadline) {
        Ok(response) => response,
        Err(message) => return Err(fail(1, &message)),
    };
    let envelope: VersionEnvelope = match serde_json::from_str(&response) {
        Ok(envelope) => envelope,
        Err(_) => return Err(fail(1, "daemon returned an invalid response")),
    };
    if envelope.version != PROTOCOL_VERSION {
        return Err(fail(
            5,
            &format!(
                "IPC protocol mismatch: daemon uses {}, CLI uses {}",
                envelope.version, PROTOCOL_VERSION
            ),
        ));
    }
    match serde_json::from_str(&response) {
        Ok(response) => Ok(response),
        Err(_) => Err(fail(1, "daemon returned an invalid response")),
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
            println!("{}", response.message);
        }
        ExitCode::SUCCESS
    } else {
        fail(4, &response.message)
    }
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
        DEFAULT_PAGE_SIZE, RenderStyle, pagination_prompt, render_history_noninteractive, render_page,
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


/// One rendered doctor line: `label  value  STATUS`, with an optional runnable
/// action (shown indented only on FAIL) and reasoning (shown only under
/// --verbose). Capability findings, the focus guard, and the provider-key
/// checks are all reduced to this single shape so the terseness contract holds
/// for every check, not just the first group.
struct DoctorRow {
    label: String,
    value: Option<String>,
    status: ReadinessStatus,
    action: Option<String>,
    detail: String,
}

impl DoctorRow {
    fn new(label: impl Into<String>, status: ReadinessStatus, detail: impl Into<String>) -> Self {
        Self { label: label.into(), value: None, status, action: None, detail: detail.into() }
    }

    fn value(mut self, value: impl Into<String>) -> Self {
        self.value = Some(value.into());
        self
    }

    fn action(mut self, action: impl Into<String>) -> Self {
        self.action = Some(action.into());
        self
    }
}

fn doctor(verbose: bool) -> ExitCode {
    let mut rows: Vec<DoctorRow> = FedoraReadiness
        .inspect()
        .into_iter()
        .map(|finding| DoctorRow {
            label: finding.capability.cli_label().to_owned(),
            value: finding.value,
            status: finding.status,
            action: finding.action,
            detail: finding.detail,
        })
        .collect();

    // One runtime serves both the focus probe and the live per-provider key
    // round trips, so the CLI never needs an ambient async runtime.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok();
    let focus_backend = runtime
        .as_ref()
        .map(|runtime| runtime.block_on(voisu_app::focus::detect_focus_backend()))
        .unwrap_or(voisu_app::focus::FocusBackendKind::None);
    rows.push(focus_guard_row(focus_backend));

    // The live key checks reach the network; a test that pins other doctor
    // output opts out with VOISU_TEST_SKIP_DOCTOR_KEYS and covers the key
    // classification through dedicated seams instead.
    if std::env::var_os("VOISU_TEST_SKIP_DOCTOR_KEYS").is_none() {
        if let Some(runtime) = runtime.as_ref() {
            rows.extend(provider_key_rows(runtime));
        }
    }

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
const DOCTOR_LABEL_WIDTH: usize = 15;
const DOCTOR_VALUE_WIDTH: usize = 20;

/// Print every doctor check as one terse line. A runnable action prints on its
/// own indented line only on FAIL; WARN and SKIP keep to one line, with any
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
        if row.status == ReadinessStatus::Fail {
            if let Some(action) = &row.action {
                println!("    {action}");
            }
        }
        if verbose {
            println!("    ({})", row.detail);
        }
    }
}

fn focus_guard_row(backend: voisu_app::focus::FocusBackendKind) -> DoctorRow {
    if backend == voisu_app::focus::FocusBackendKind::None {
        DoctorRow::new(
            "Focus guard",
            ReadinessStatus::Warn,
            "no focus backend; guarded Delivery fails closed to the clipboard",
        )
        .value("none")
    } else {
        DoctorRow::new(
            "Focus guard",
            ReadinessStatus::Pass,
            "focus backend available for guarded Delivery",
        )
        .value(backend.as_str())
    }
}

/// Live per-provider key round trips as structured rows. A wrong key (401/403)
/// is a hard failure naming the fix; a missing key, quota, throttle, or
/// unreachable provider is a warning, since none of those is a definitively bad
/// key; a disabled provider is skipped.
fn provider_key_rows(runtime: &tokio::runtime::Runtime) -> Vec<DoctorRow> {
    let mut rows = Vec::new();
    let deepgram_enabled = voisu_app::config::deepgram_enabled();
    for provider in [Provider::Deepgram, Provider::Groq] {
        let label = format!("{} key", provider.cli_label());
        if provider == Provider::Deepgram && !deepgram_enabled {
            rows.push(
                DoctorRow::new(
                    label,
                    ReadinessStatus::Skip,
                    "Deepgram is off; run `voisu deepgram on` to enable",
                )
                .value("off"),
            );
            continue;
        }
        let row = match SecretToolStore.diagnose(provider) {
            KeyDiagnosis::Found { location, credential } => {
                let status = runtime.block_on(ProviderHttpClient.check(provider, credential));
                let location_note = match location {
                    KeyLocation::PlaintextFile => {
                        " — stored in the plaintext fallback file; run `voisu setup` to migrate it into your keyring"
                    }
                    KeyLocation::EnvOverride => {
                        " — provided by the environment override, which wins over any stored key"
                    }
                    KeyLocation::Keyring => "",
                };
                let value = match status {
                    ProviderKeyStatus::Valid => "valid",
                    ProviderKeyStatus::InvalidKey => "invalid",
                    ProviderKeyStatus::RateLimited => "rate-limited",
                    ProviderKeyStatus::QuotaExhausted => "quota exhausted",
                    ProviderKeyStatus::Unreachable => "unreachable",
                };
                let mut detail = format!("{}{}", status.headline(), location_note);
                let mut row = DoctorRow::new(label, status.readiness(), String::new()).value(value);
                match status {
                    ProviderKeyStatus::InvalidKey => {
                        detail = format!("{detail}; {}", provider_free_tier_hint(provider));
                        row = row.action("run `voisu setup`");
                    }
                    ProviderKeyStatus::QuotaExhausted => {
                        detail = format!("{detail}; {}", provider_free_tier_hint(provider));
                    }
                    _ => {}
                }
                row.detail = detail;
                row
            }
            // A present env override always wins at runtime, so a malformed one
            // shadows every stored key and breaks dictation: a hard failure
            // naming the variable, never a PASS on the shadowed key.
            KeyDiagnosis::EnvOverrideInvalid => {
                let variable = provider.environment_variable();
                DoctorRow::new(
                    label,
                    ReadinessStatus::Fail,
                    format!(
                        "{variable} is set but is not a usable key (empty or contains a line \
                         break) and it overrides any stored key"
                    ),
                )
                .value("env override invalid")
                .action(format!("unset or fix {variable}"))
            }
            KeyDiagnosis::Absent => DoctorRow::new(
                label,
                ReadinessStatus::Warn,
                format!(
                    "not configured — run `voisu setup`; {}",
                    provider_free_tier_hint(provider)
                ),
            )
            .value("not configured"),
            // The keyring could not be consulted: steer the user at the real fix
            // (unlock / start / install) rather than telling them to write a
            // plaintext key they may not need to.
            KeyDiagnosis::Locked => DoctorRow::new(
                label,
                ReadinessStatus::Warn,
                "keyring locked — unlock it, or run `voisu setup` to store a key",
            )
            .value("keyring locked"),
            KeyDiagnosis::Unavailable => DoctorRow::new(
                label,
                ReadinessStatus::Warn,
                "no desktop keyring available — run `voisu setup` to store a key",
            )
            .value("no keyring"),
            KeyDiagnosis::ToolMissing => DoctorRow::new(
                label,
                ReadinessStatus::Warn,
                "secret-tool is not installed — install libsecret-tools, or run `voisu setup`",
            )
            .value("secret-tool missing"),
        };
        rows.push(row);
    }
    rows
}

/// `voisu setup` — the interactive, re-runnable wizard that validates each key
/// live before storing it. All logic lives in the injectable `voisu_app::setup`
/// core; here we supply the real terminal, keyring, and live validator.
fn setup() -> ExitCode {
    use voisu_app::setup::{LiveKeyValidator, ProviderOutcome, StdioWizard, run_setup};
    let outcome = run_setup(&mut StdioWizard, &mut SecretToolStore, &mut LiveKeyValidator);
    // A run that stored/kept no usable key (every provider skipped or its store
    // failed) did not accomplish setup's job, so it must not report success.
    let usable = |outcome: ProviderOutcome| {
        matches!(
            outcome,
            ProviderOutcome::Stored | ProviderOutcome::Kept | ProviderOutcome::StoredUnverified
        )
    };
    if usable(outcome.deepgram) || usable(outcome.groq) {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(4)
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
        println!("delivery mode: {}", voisu_app::config::delivery_mode().as_str());
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
        .map_err(|_| BoundaryError::new(BoundaryKind::SecretStorage, "cannot read credential from standard input"))?;
    Credential::new(credential.trim_end().to_owned())
}

fn auth_verify(provider: Provider) -> ExitCode {
    let credential = match SecretToolStore.load(provider) {
        Ok(credential) => credential,
        Err(error) => return fail(4, error.public_message()),
    };
    let mut authenticator = ProviderHttpClient;
    match block_on(ProviderAuthenticator::verify(&mut authenticator, provider, credential)) {
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
        .map_err(|_| BoundaryError::new(BoundaryKind::ProviderAuthentication, "runtime unavailable"))?
        .block_on(future)
}

fn read_response_frame(stream: &mut UnixStream, deadline: Duration) -> Result<String, String> {
    let started = Instant::now();
    let mut response = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        let remaining = deadline
            .checked_sub(started.elapsed())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| "daemon response deadline elapsed".to_owned())?;
        stream
            .set_read_timeout(Some(remaining))
            .map_err(|_| "failed to configure daemon connection deadline".to_owned())?;
        match stream.read(&mut buffer) {
            Ok(0) => return Err("daemon response frame is incomplete".to_owned()),
            Ok(read) => {
                response.extend_from_slice(&buffer[..read]);
                if response.len() as u64 > MAX_RESPONSE_BYTES {
                    return Err("daemon response frame is too large".to_owned());
                }
                if response.ends_with(b"\n") {
                    return String::from_utf8(response)
                        .map_err(|_| "daemon returned an invalid response".to_owned());
                }
                if response.contains(&b'\n') {
                    return Err("daemon response frame is malformed".to_owned());
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err("daemon response deadline elapsed".to_owned());
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return Err("failed to read daemon response".to_owned()),
        }
    }
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
        [command, correlation_id] if command == "export" => {
            Ok(CliAction::Daemon(Command::Export(ExportCorrelationId::new(
                correlation_id.clone(),
            ))))
        }
        [command, path] if command == "replay" => {
            Ok(CliAction::Daemon(Command::Replay(ReplayFixturePath::new(path.clone()))))
        }
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
        [command] if command == "delivery" => Ok(CliAction::Delivery(None)),
        [command, mode] if command == "delivery" => {
            Ok(CliAction::Delivery(Some(parse_delivery_mode(mode)?)))
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

fn parse_toggle(value: &str) -> Result<bool, String> {
    match value {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err("deepgram must be on or off".to_owned()),
    }
}

fn parse_delivery_mode(value: &str) -> Result<DeliveryMode, String> {
    match value {
        "type" => Ok(DeliveryMode::Type),
        "clipboard" => Ok(DeliveryMode::Clipboard),
        "guarded" => Ok(DeliveryMode::Guarded),
        _ => Err("delivery mode must be type, clipboard, or guarded".to_owned()),
    }
}

fn parse_provider(value: &str) -> Result<Provider, String> {
    match value {
        "groq" => Ok(Provider::Groq),
        "deepgram" => Ok(Provider::Deepgram),
        _ => Err("provider must be groq or deepgram".to_owned()),
    }
}

fn usage() -> &'static str {
    "usage: voisu <setup|start|stop|toggle|status|shortcut|history|export|replay|doctor|auth|deepgram|delivery|dictionary|service>\n\n  voisu setup  # guided, re-runnable wizard: validate and store your Deepgram and Groq keys\n  voisu shortcut  # show the desktop-approved Trigger Key binding\n  voisu history  # newest-first Recordings with per-Provider outcome and tail latency\n  voisu history --json  # the full raw diagnostic records as JSON\n  voisu export <correlation-id>\n  voisu replay <fixture-name>  # a file inside the private fixtures directory\n  voisu doctor [--verbose]  # capability, focus-guard, and live per-key round-trip checks; --verbose adds the reasoning behind each line\n  voisu auth set <groq|deepgram>  # credential is read from stdin\n  voisu auth verify <groq|deepgram>\n  voisu deepgram <on|off>  # enable/disable the Deepgram Provider (default on)\n  voisu delivery [type|clipboard|guarded]  # choose Transcript Delivery (default type); no argument shows the persisted mode\n  voisu dictionary add <term>\n  voisu dictionary remove <term>\n  voisu dictionary list [--json]\n  voisu service <install|start|stop|restart|status|uninstall>"
}

fn fail(code: u8, message: &str) -> ExitCode {
    eprintln!("{message}");
    ExitCode::from(code)
}
