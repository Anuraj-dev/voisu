use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use voisu_core::{
    BoundaryError, BoundaryFuture, BoundaryKind, Command, Credential, PROTOCOL_VERSION, Provider,
    ProviderAuthenticator, ReadinessInspector, ReadinessStatus, Request, Response, SecretStore,
    VersionEnvelope, socket_path,
};
use voisu_app::system::{FedoraReadiness, ProviderHttpClient, SecretToolStore};

const MAX_RESPONSE_BYTES: u64 = 16 * 1024;
const IO_DEADLINE: Duration = Duration::from_secs(2);
const PROCESSING_RESPONSE_DEADLINE: Duration = Duration::from_secs(17);

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

    let response_deadline = if matches!(command, Command::Stop | Command::Toggle) {
        PROCESSING_RESPONSE_DEADLINE
    } else {
        IO_DEADLINE
    };
    let response = match read_response_frame(&mut stream, response_deadline) {
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
    let findings = FedoraReadiness.inspect();
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
    match SecretToolStore.replace(provider, credential) {
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
