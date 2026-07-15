use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command as ProcessCommand, ExitCode, Stdio};
use std::time::{Duration, Instant};

use voisu_core::{
    BoundaryError, BoundaryFuture, BoundaryKind, Command, Credential, PROTOCOL_VERSION,
    Provider, ProviderAuthenticator, ReadinessCapability, ReadinessFinding, ReadinessInspector,
    ReadinessStatus, Request, Response, SecretStore, VersionEnvelope, socket_path,
};

const MAX_RESPONSE_BYTES: u64 = 16 * 1024;
const IO_DEADLINE: Duration = Duration::from_secs(2);

enum CliAction {
    Daemon(Command),
    Doctor,
    AuthSet(Provider),
    AuthVerify(Provider),
    Help,
}

fn main() -> ExitCode {
    match parse_command() {
        Ok(CliAction::Daemon(command)) => daemon_command(command),
        Ok(CliAction::Doctor) => doctor(),
        Ok(CliAction::AuthSet(provider)) => match credential_from_stdin() {
            Ok(credential) => auth_set(provider, credential),
            Err(error) => fail(2, error.public_message()),
        },
        Ok(CliAction::AuthVerify(provider)) => auth_verify(provider),
        Ok(CliAction::Help) => {
            println!("{}", usage());
            ExitCode::SUCCESS
        }
        Err(message) => fail(2, &message),
    }
}

fn daemon_command(command: Command) -> ExitCode {
    let path = match socket_path() {
        Ok(path) => path,
        Err(message) => return fail(2, &message),
    };
    let mut stream = match UnixStream::connect(path) {
        Ok(stream) => stream,
        Err(_) => {
            println!("daemon unavailable");
            return ExitCode::from(3);
        }
    };

    if stream.set_write_timeout(Some(IO_DEADLINE)).is_err() {
        return fail(1, "failed to configure daemon connection deadline");
    }

    let request = Request {
        version: PROTOCOL_VERSION,
        command,
    };
    if serde_json::to_writer(&mut stream, &request).is_err() || stream.write_all(b"\n").is_err() {
        return fail(1, "failed to send command to daemon");
    }

    let response = match read_response_frame(&mut stream) {
        Ok(response) => response,
        Err(message) => return fail(1, &message),
    };
    let envelope: VersionEnvelope = match serde_json::from_str(&response) {
        Ok(envelope) => envelope,
        Err(_) => return fail(1, "daemon returned an invalid response"),
    };
    if envelope.version != PROTOCOL_VERSION {
        return fail(
            5,
            &format!(
                "IPC protocol mismatch: daemon uses {}, CLI uses {}",
                envelope.version, PROTOCOL_VERSION
            ),
        );
    }
    let response: Response = match serde_json::from_str(&response) {
        Ok(response) => response,
        Err(_) => return fail(1, "daemon returned an invalid response"),
    };
    if response.ok {
        println!("{}", response.message);
        ExitCode::SUCCESS
    } else {
        fail(4, &response.message)
    }
}

fn doctor() -> ExitCode {
    let findings = SystemReadiness.inspect();
    let has_failure = findings.iter().any(|finding| finding.status == ReadinessStatus::Fail);
    for finding in findings {
        println!(
            "{}: {} ({})",
            finding.capability.cli_label(),
            finding.status.cli_label(),
            finding.detail
        );
    }
    if has_failure {
        ExitCode::from(4)
    } else {
        ExitCode::SUCCESS
    }
}

fn auth_set(provider: Provider, credential: Credential) -> ExitCode {
    match SystemSecretStore.replace(provider, credential) {
        Ok(()) => {
            println!("{} credential stored", provider.cli_label());
            ExitCode::SUCCESS
        }
        Err(error) => fail(4, error.public_message()),
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
    let credential = match SystemSecretStore.load(provider) {
        Ok(credential) => credential,
        Err(error) => return fail(4, error.public_message()),
    };
    match block_on(SystemProviderAuthenticator.verify(provider, credential)) {
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

fn read_response_frame(stream: &mut UnixStream) -> Result<String, String> {
    let started = Instant::now();
    let mut response = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        let remaining = IO_DEADLINE
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
        [command] if command == "doctor" => Ok(CliAction::Doctor),
        [command] if command == "--help" || command == "-h" || command == "help" => {
            Ok(CliAction::Help)
        }
        [auth, set, provider] if auth == "auth" && set == "set" => {
            Ok(CliAction::AuthSet(parse_provider(provider)?))
        }
        [auth, verify, provider] if auth == "auth" && verify == "verify" => {
            Ok(CliAction::AuthVerify(parse_provider(provider)?))
        }
        _ => Err(usage().to_owned()),
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
    "usage: voisu <start|stop|toggle|status|doctor|auth>\n\n  voisu doctor\n  voisu auth set <groq|deepgram>  # credential is read from stdin\n  voisu auth verify <groq|deepgram>"
}

fn fail(code: u8, message: &str) -> ExitCode {
    eprintln!("{message}");
    ExitCode::from(code)
}

struct SystemReadiness;

impl ReadinessInspector for SystemReadiness {
    fn inspect(&mut self) -> Vec<ReadinessFinding> {
        if let Some(value) = std::env::var_os("VOISU_TEST_READINESS") {
            return controlled_readiness(&value.to_string_lossy());
        }
        vec![
            command_finding(ReadinessCapability::PipeWire, "pw-cli", &["info", "0"], "available", "not available"),
            command_output_finding(
                ReadinessCapability::Microphone,
                "wpctl",
                &["status"],
                "present",
                "not detected",
                |output| output.contains("Sources"),
            ),
            command_output_finding(
                ReadinessCapability::Portals,
                "busctl",
                &["--user", "--no-pager", "list"],
                "available",
                "not available",
                |output| output.contains("org.freedesktop.portal.Desktop"),
            ),
            command_finding(ReadinessCapability::Clipboard, "wl-copy", &["--version"], "available", "not available"),
            command_finding(ReadinessCapability::SecretStorage, "secret-tool", &["--help"], "available", "not available"),
            daemon_finding(),
        ]
    }
}

fn controlled_readiness(value: &str) -> Vec<ReadinessFinding> {
    let mut findings = vec![
        readiness(ReadinessCapability::PipeWire, ReadinessStatus::Pass, "available"),
        readiness(ReadinessCapability::Microphone, ReadinessStatus::Pass, "present"),
        readiness(ReadinessCapability::Portals, ReadinessStatus::Pass, "available"),
        readiness(ReadinessCapability::Clipboard, ReadinessStatus::Pass, "available"),
        readiness(ReadinessCapability::SecretStorage, ReadinessStatus::Pass, "available"),
        daemon_finding(),
    ];
    if value == "pass" {
        return findings;
    }
    for override_value in value.split(',') {
        let Some((capability, status)) = override_value.split_once('=') else {
            continue;
        };
        let (status, detail) = match status {
            "warn" => (ReadinessStatus::Warn, "needs attention"),
            "fail" => (ReadinessStatus::Fail, "not available"),
            _ => continue,
        };
        if let Some(finding) = findings.iter_mut().find(|finding| {
            matches!(
                (capability, finding.capability),
                ("pipewire", ReadinessCapability::PipeWire)
                    | ("microphone", ReadinessCapability::Microphone)
                    | ("portals", ReadinessCapability::Portals)
                    | ("clipboard", ReadinessCapability::Clipboard)
                    | ("secret-storage", ReadinessCapability::SecretStorage)
                    | ("daemon", ReadinessCapability::Daemon)
            )
        }) {
            finding.status = status;
            finding.detail = detail.to_owned();
        }
    }
    findings
}

fn readiness(
    capability: ReadinessCapability,
    status: ReadinessStatus,
    detail: &str,
) -> ReadinessFinding {
    ReadinessFinding {
        capability,
        status,
        detail: detail.to_owned(),
    }
}

fn command_finding(
    capability: ReadinessCapability,
    command: &str,
    arguments: &[&str],
    pass_detail: &str,
    fail_detail: &str,
) -> ReadinessFinding {
    let available = ProcessCommand::new(command)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    readiness(
        capability,
        if available { ReadinessStatus::Pass } else { ReadinessStatus::Fail },
        if available { pass_detail } else { fail_detail },
    )
}

fn command_output_finding(
    capability: ReadinessCapability,
    command: &str,
    arguments: &[&str],
    pass_detail: &str,
    fail_detail: &str,
    matches_required_capability: impl FnOnce(&str) -> bool,
) -> ReadinessFinding {
    let available = ProcessCommand::new(command)
        .args(arguments)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|output| {
            output.status.success()
                && matches_required_capability(&String::from_utf8_lossy(&output.stdout))
        });
    readiness(
        capability,
        if available { ReadinessStatus::Pass } else { ReadinessStatus::Fail },
        if available { pass_detail } else { fail_detail },
    )
}

fn daemon_finding() -> ReadinessFinding {
    let reachable = socket_path()
        .ok()
        .and_then(|path| UnixStream::connect(path).ok())
        .is_some();
    readiness(
        ReadinessCapability::Daemon,
        if reachable { ReadinessStatus::Pass } else { ReadinessStatus::Fail },
        if reachable { "reachable" } else { "unavailable" },
    )
}

struct SystemSecretStore;

impl SecretStore for SystemSecretStore {
    fn replace(&mut self, provider: Provider, credential: Credential) -> Result<(), BoundaryError> {
        if let Some(mode) = std::env::var_os("VOISU_TEST_SECRET_STORE") {
            return controlled_secret_store(&mode.to_string_lossy());
        }
        let provider_value = provider.secret_service_value();
        let mut store = ProcessCommand::new("secret-tool")
            .args(["store", "--label=Voisu cloud credential", "voisu-provider", provider_value])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| BoundaryError::new(BoundaryKind::SecretStorage, "secret-tool unavailable"))?;
        let stdin = store.stdin.as_mut().ok_or_else(|| {
            BoundaryError::new(BoundaryKind::SecretStorage, "secret-tool input is unavailable")
        })?;
        stdin
            .write_all(credential.expose_to_boundary().as_bytes())
            .and_then(|_| stdin.write_all(b"\n"))
            .map_err(|_| BoundaryError::new(BoundaryKind::SecretStorage, "secret-tool rejected credential input"))?;
        if store.wait().is_ok_and(|status| status.success()) {
            Ok(())
        } else {
            Err(BoundaryError::new(BoundaryKind::SecretStorage, "secret service denied credential storage"))
        }
    }

    fn load(&mut self, provider: Provider) -> Result<Credential, BoundaryError> {
        // This is the explicit, non-persistent fallback for development and
        // headless use. It takes precedence so a denied desktop service never
        // blocks the documented fallback path.
        if let Some(credential) = std::env::var_os(provider.environment_variable()) {
            return Credential::new(credential.to_string_lossy().into_owned());
        }
        if let Some(mode) = std::env::var_os("VOISU_TEST_SECRET_STORE") {
            if mode == "available" {
                let name = match provider {
                    Provider::Groq => "VOISU_TEST_STORED_GROQ_CREDENTIAL",
                    Provider::Deepgram => "VOISU_TEST_STORED_DEEPGRAM_CREDENTIAL",
                };
                return std::env::var(name)
                    .map_err(|_| BoundaryError::new(BoundaryKind::SecretStorage, "controlled credential missing"))
                    .and_then(Credential::new);
            }
            return controlled_secret_store(&mode.to_string_lossy()).and_then(|()| {
                Err(BoundaryError::new(BoundaryKind::SecretStorage, "controlled credential missing"))
            });
        }
        let output = ProcessCommand::new("secret-tool")
            .args(["lookup", "voisu-provider", provider.secret_service_value()])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(|_| BoundaryError::new(BoundaryKind::SecretStorage, "secret-tool unavailable"))?;
        if !output.status.success() {
            return Err(BoundaryError::new(BoundaryKind::SecretStorage, "secret service lookup denied"));
        }
        let credential = String::from_utf8(output.stdout)
            .map_err(|_| BoundaryError::new(BoundaryKind::SecretStorage, "secret service returned invalid data"))?;
        Credential::new(credential.trim_end().to_owned())
    }
}

fn controlled_secret_store(mode: &str) -> Result<(), BoundaryError> {
    if mode == "available" {
        Ok(())
    } else {
        Err(BoundaryError::new(BoundaryKind::SecretStorage, "controlled secret service denied access"))
    }
}

struct SystemProviderAuthenticator;

impl ProviderAuthenticator for SystemProviderAuthenticator {
    fn verify(&mut self, provider: Provider, credential: Credential) -> BoundaryFuture<'_, ()> {
        Box::pin(async move {
            let controlled = match provider {
                Provider::Groq => std::env::var_os("VOISU_TEST_AUTH_GROQ"),
                Provider::Deepgram => std::env::var_os("VOISU_TEST_AUTH_DEEPGRAM"),
            };
            if let Some(result) = controlled {
                return if result == "authorized" {
                    Ok(())
                } else {
                    Err(BoundaryError::new(BoundaryKind::ProviderAuthentication, "controlled provider rejected credential"))
                };
            }
            let (url, header_prefix) = match provider {
                Provider::Groq => ("https://api.groq.com/openai/v1/models", "Bearer"),
                Provider::Deepgram => ("https://api.deepgram.com/v1/projects", "Token"),
            };
            let config = format!(
                "url = \"{url}\"\nheader = \"Authorization: {header_prefix} {}\"\n",
                credential.expose_to_boundary()
            );
            let mut curl = ProcessCommand::new("curl")
                .args(["--config", "-", "--fail", "--silent", "--show-error", "--output", "/dev/null", "--max-time", "5"])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|_| BoundaryError::new(BoundaryKind::ProviderAuthentication, "curl unavailable"))?;
            let stdin = curl.stdin.as_mut().ok_or_else(|| {
                BoundaryError::new(BoundaryKind::ProviderAuthentication, "curl input unavailable")
            })?;
            stdin
                .write_all(config.as_bytes())
                .map_err(|_| BoundaryError::new(BoundaryKind::ProviderAuthentication, "curl rejected credential input"))?;
            if curl.wait().is_ok_and(|status| status.success()) {
                Ok(())
            } else {
                Err(BoundaryError::new(BoundaryKind::ProviderAuthentication, "provider rejected credential"))
            }
        })
    }
}
