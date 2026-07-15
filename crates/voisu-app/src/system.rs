use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use voisu_core::{
    socket_path, BoundaryError, BoundaryFuture, BoundaryKind, Command as DaemonCommand, Credential,
    Provider, ProviderAuthenticator, ReadinessCapability, ReadinessFinding, ReadinessInspector,
    ReadinessStatus, Request, Response, SecretStore, VersionEnvelope, PROTOCOL_VERSION,
};

const PROCESS_DEADLINE: Duration = Duration::from_secs(2);
const PROCESS_POLL: Duration = Duration::from_millis(10);
const MAX_DAEMON_RESPONSE_BYTES: usize = 16 * 1024;

pub struct FedoraReadiness;

impl ReadinessInspector for FedoraReadiness {
    fn inspect(&mut self) -> Vec<ReadinessFinding> {
        if let Some(value) = std::env::var_os("VOISU_TEST_READINESS") {
            return controlled_readiness(&value.to_string_lossy());
        }
        vec![
            command_finding(
                ReadinessCapability::PipeWire,
                "pw-cli",
                &["info", "0"],
                "PipeWire core responds",
                "start PipeWire and WirePlumber",
            ),
            microphone_finding(),
            command_finding(
                ReadinessCapability::Portals,
                "busctl",
                &["--user", "--no-pager", "status", "org.freedesktop.portal.Desktop"],
                "desktop portal responds",
                "start xdg-desktop-portal in this desktop session",
            ),
            clipboard_finding(),
            secret_service_finding(),
            daemon_finding(),
        ]
    }
}

pub struct SecretToolStore;

impl SecretStore for SecretToolStore {
    fn replace(&mut self, provider: Provider, credential: Credential) -> Result<(), BoundaryError> {
        if let Some(mode) = std::env::var_os("VOISU_TEST_SECRET_STORE") {
            return controlled_secret_store(&mode.to_string_lossy());
        }
        let outcome = run_restricted(
            "secret-tool",
            &["store", "--label=Voisu cloud credential", "voisu-provider", provider.secret_service_value()],
            Some(credential.expose_to_boundary().as_bytes()),
            false,
        )
        .map_err(secret_storage_error)?;
        if outcome.success {
            Ok(())
        } else {
            Err(BoundaryError::new(
                BoundaryKind::SecretStorage,
                "secret service denied credential storage",
            ))
        }
    }

    fn load(&mut self, provider: Provider) -> Result<Credential, BoundaryError> {
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
        let outcome = run_restricted(
            "secret-tool",
            &["lookup", "voisu-provider", provider.secret_service_value()],
            None,
            true,
        )
        .map_err(secret_storage_error)?;
        if !outcome.success {
            return Err(BoundaryError::new(
                BoundaryKind::SecretStorage,
                "secret service lookup denied",
            ));
        }
        let credential = String::from_utf8(outcome.stdout).map_err(|_| {
            BoundaryError::new(BoundaryKind::SecretStorage, "secret service returned invalid data")
        })?;
        Credential::new(credential.trim_end().to_owned())
    }
}

pub struct ProviderHttpClient;

/// A credentialed provider request with no response body retained. The next
/// provider adapter can supply its own endpoint while reusing this process and
/// environment boundary.
pub struct ProviderHttpRequest {
    pub url: &'static str,
    pub authorization_scheme: &'static str,
}

impl ProviderHttpClient {
    /// Runs the shared authenticated provider request boundary and returns only
    /// its HTTP status. Future Groq transcription can reuse this async boundary
    /// without inheriting credentials or curl configuration from the CLI.
    pub async fn authenticated_status(
        &self,
        credential: Credential,
        request: ProviderHttpRequest,
    ) -> Result<u16, BoundaryError> {
        let result = tokio::task::spawn_blocking(move || authenticated_status(credential, request))
            .await
            .map_err(|_| BoundaryError::new(BoundaryKind::ProviderAuthentication, "provider request task failed"))?;
        result
    }

    pub async fn verify(&self, provider: Provider, credential: Credential) -> Result<(), BoundaryError> {
        let request = match provider {
            Provider::Groq => ProviderHttpRequest {
                url: "https://api.groq.com/openai/v1/models",
                authorization_scheme: "Bearer",
            },
            Provider::Deepgram => ProviderHttpRequest {
                url: "https://api.deepgram.com/v1/projects",
                authorization_scheme: "Token",
            },
        };
        let status = self.authenticated_status(credential, request).await?;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(BoundaryError::new(
                BoundaryKind::ProviderAuthentication,
                "provider returned a non-success HTTP status",
            ))
        }
    }
}

impl ProviderAuthenticator for ProviderHttpClient {
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
                    Err(BoundaryError::new(
                        BoundaryKind::ProviderAuthentication,
                        "controlled provider rejected credential",
                    ))
                };
            }
            ProviderHttpClient::verify(&ProviderHttpClient, provider, credential).await
        })
    }
}

fn authenticated_status(
    credential: Credential,
    request: ProviderHttpRequest,
) -> Result<u16, BoundaryError> {
    let credential = curl_config_escape(credential.expose_to_boundary());
    let config = format!(
        "url = \"{}\"\nheader = \"Authorization: {} {credential}\"\n",
        request.url, request.authorization_scheme,
    );
    let outcome = run_restricted(
        "curl",
        &[
            "-q",
            "--config",
            "-",
            "--fail",
            "--silent",
            "--show-error",
            "--output",
            "/dev/null",
            "--write-out",
            "%{http_code}",
            "--max-time",
            "2",
        ],
        Some(config.as_bytes()),
        true,
    )
    .map_err(provider_authentication_error)?;
    if !outcome.success {
        return Err(BoundaryError::new(
            BoundaryKind::ProviderAuthentication,
            "provider rejected credential",
        ));
    }
    let status = std::str::from_utf8(&outcome.stdout)
        .ok()
        .and_then(|value| value.trim().parse::<u16>().ok())
        .ok_or_else(|| {
            BoundaryError::new(BoundaryKind::ProviderAuthentication, "provider returned no HTTP status")
        })?;
    Ok(status)
}

fn curl_config_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn secret_storage_error(error: ProcessError) -> BoundaryError {
    let detail = match error {
        ProcessError::Unavailable => "secret-tool unavailable",
        ProcessError::Input => "secret-tool rejected credential input",
        ProcessError::TimedOut => "secret-tool deadline elapsed",
        ProcessError::Wait | ProcessError::Output => "secret-tool execution failed",
    };
    BoundaryError::new(BoundaryKind::SecretStorage, detail)
}

fn provider_authentication_error(error: ProcessError) -> BoundaryError {
    let detail = match error {
        ProcessError::Unavailable => "curl unavailable",
        ProcessError::Input => "curl rejected credential input",
        ProcessError::TimedOut => "curl deadline elapsed",
        ProcessError::Wait | ProcessError::Output => "curl execution failed",
    };
    BoundaryError::new(BoundaryKind::ProviderAuthentication, detail)
}

fn controlled_readiness(value: &str) -> Vec<ReadinessFinding> {
    let mut findings = vec![
        readiness(ReadinessCapability::PipeWire, ReadinessStatus::Pass, "PipeWire core responds"),
        readiness(ReadinessCapability::Microphone, ReadinessStatus::Pass, "default source available"),
        readiness(ReadinessCapability::Portals, ReadinessStatus::Pass, "desktop portal responds"),
        readiness(ReadinessCapability::Clipboard, ReadinessStatus::Pass, "clipboard roundtrip succeeds"),
        readiness(ReadinessCapability::SecretStorage, ReadinessStatus::Pass, "Secret Service responds"),
        daemon_finding(),
    ];
    if value == "pass" {
        return findings;
    }
    for override_value in value.split(',') {
        let Some((capability, status)) = override_value.split_once('=') else { continue };
        let (status, detail) = match status {
            "warn" => (ReadinessStatus::Warn, "needs attention; see remediation"),
            "fail" => (ReadinessStatus::Fail, "not available; see remediation"),
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

fn microphone_finding() -> ReadinessFinding {
    match run_restricted("wpctl", &["inspect", "@DEFAULT_AUDIO_SOURCE@"], None, true) {
        Ok(outcome) if outcome.success => readiness(
            ReadinessCapability::Microphone,
            ReadinessStatus::Pass,
            "default source available",
        ),
        Ok(_) => readiness(
            ReadinessCapability::Microphone,
            ReadinessStatus::Warn,
            "no default microphone; connect one and set it as the default source",
        ),
        Err(_) => readiness(
            ReadinessCapability::Microphone,
            ReadinessStatus::Fail,
            "WirePlumber is unavailable; start PipeWire and WirePlumber",
        ),
    }
}

fn clipboard_finding() -> ReadinessFinding {
    let original = match run_restricted("wl-paste", &["--no-newline"], None, true) {
        Ok(outcome) if outcome.success => outcome.stdout,
        _ => return readiness(
            ReadinessCapability::Clipboard,
            ReadinessStatus::Fail,
            "cannot read the Wayland clipboard; run inside an active Wayland session",
        ),
    };
    let probe = format!("voisu-readiness-{}", std::process::id());
    let copied = run_restricted("wl-copy", &["--"], Some(probe.as_bytes()), false)
        .is_ok_and(|outcome| outcome.success);
    let observed = run_restricted("wl-paste", &["--no-newline"], None, true)
        .ok()
        .filter(|outcome| outcome.success)
        .map(|outcome| outcome.stdout == probe.as_bytes())
        .unwrap_or(false);
    let restored = run_restricted("wl-copy", &["--"], Some(&original), false)
        .is_ok_and(|outcome| outcome.success);
    match (copied && observed, restored) {
        (true, true) => readiness(
            ReadinessCapability::Clipboard,
            ReadinessStatus::Pass,
            "clipboard roundtrip succeeds and the prior clipboard was restored",
        ),
        (true, false) => readiness(
            ReadinessCapability::Clipboard,
            ReadinessStatus::Warn,
            "clipboard roundtrip succeeds but the prior clipboard could not be restored",
        ),
        _ => readiness(
            ReadinessCapability::Clipboard,
            ReadinessStatus::Fail,
            "clipboard roundtrip failed; install wl-clipboard and use an active Wayland session",
        ),
    }
}

fn secret_service_finding() -> ReadinessFinding {
    // Probe a nonexistent attribute. On a healthy, unlocked keyring this exits
    // without a match and without diagnostics: reaching the service cleanly is
    // the readiness signal, not whether a credential was found. Real secret-tool
    // reports a no-match with a nonzero exit and empty stdout/stderr, while a
    // D-Bus/service failure or a locked keyring prints an error to stderr.
    let probe = std::process::id().to_string();
    match run_restricted("secret-tool", &["lookup", "voisu-doctor-probe", &probe], None, false) {
        Ok(outcome) if outcome.success || outcome.stderr.is_empty() => readiness(
            ReadinessCapability::SecretStorage,
            ReadinessStatus::Pass,
            "Secret Service is reachable",
        ),
        Ok(_) => readiness(
            ReadinessCapability::SecretStorage,
            ReadinessStatus::Warn,
            "Secret Service reported an error; unlock the keyring or log in to the desktop session",
        ),
        Err(_) => readiness(
            ReadinessCapability::SecretStorage,
            ReadinessStatus::Fail,
            "Secret Service is unavailable; start or unlock the desktop keyring",
        ),
    }
}

fn command_finding(
    capability: ReadinessCapability,
    command: &str,
    arguments: &[&str],
    pass_detail: &str,
    fail_detail: &str,
) -> ReadinessFinding {
    let available = run_restricted(command, arguments, None, false)
        .is_ok_and(|outcome| outcome.success);
    readiness(
        capability,
        if available { ReadinessStatus::Pass } else { ReadinessStatus::Fail },
        if available { pass_detail } else { fail_detail },
    )
}

fn daemon_finding() -> ReadinessFinding {
    let result = daemon_status_handshake();
    readiness(
        ReadinessCapability::Daemon,
        if result.is_ok() { ReadinessStatus::Pass } else { ReadinessStatus::Fail },
        if result.is_ok() {
            "status handshake succeeds"
        } else {
            "daemon status handshake failed; start voisu-daemon and run voisu doctor again"
        },
    )
}

fn daemon_status_handshake() -> Result<(), ()> {
    let path = socket_path().map_err(|_| ())?;
    let mut stream = UnixStream::connect(path).map_err(|_| ())?;
    // A single Instant budget bounds the whole handshake. A per-read timeout is
    // reset by every byte, so a peer trickling one byte per interval would hold
    // doctor forever; the accumulated response is also capped during reading so
    // an oversized frame can never be fully buffered before the cap is checked.
    let started = Instant::now();
    stream.set_write_timeout(Some(PROCESS_DEADLINE)).map_err(|_| ())?;
    serde_json::to_writer(&mut stream, &Request { version: PROTOCOL_VERSION, command: DaemonCommand::Status })
        .map_err(|_| ())?;
    stream.write_all(b"\n").map_err(|_| ())?;
    let response = read_bounded_frame(&mut stream, started)?;
    let envelope: VersionEnvelope = serde_json::from_str(&response).map_err(|_| ())?;
    let response: Response = serde_json::from_str(&response).map_err(|_| ())?;
    (envelope.version == PROTOCOL_VERSION && response.ok && response.state.is_some())
        .then_some(())
        .ok_or(())
}

fn read_bounded_frame(stream: &mut UnixStream, started: Instant) -> Result<String, ()> {
    let mut response = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        let remaining = PROCESS_DEADLINE
            .checked_sub(started.elapsed())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(())?;
        stream.set_read_timeout(Some(remaining)).map_err(|_| ())?;
        match stream.read(&mut buffer) {
            Ok(0) => return Err(()),
            Ok(read) => {
                response.extend_from_slice(&buffer[..read]);
                if response.len() > MAX_DAEMON_RESPONSE_BYTES {
                    return Err(());
                }
                if response.ends_with(b"\n") {
                    return String::from_utf8(response).map_err(|_| ());
                }
                if response.contains(&b'\n') {
                    return Err(());
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return Err(()),
        }
    }
}

fn readiness(capability: ReadinessCapability, status: ReadinessStatus, detail: &str) -> ReadinessFinding {
    ReadinessFinding { capability, status, detail: detail.to_owned() }
}

fn controlled_secret_store(mode: &str) -> Result<(), BoundaryError> {
    if mode == "available" {
        Ok(())
    } else {
        Err(BoundaryError::new(BoundaryKind::SecretStorage, "controlled secret service denied access"))
    }
}

struct ProcessOutcome {
    success: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

enum ProcessError {
    Unavailable,
    Input,
    TimedOut,
    Wait,
    Output,
}

fn restricted_command(program: &str) -> Command {
    let mut command = Command::new(program);
    command.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    for name in ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
}

fn run_restricted(
    program: &str,
    arguments: &[&str],
    input: Option<&[u8]>,
    capture_stdout: bool,
) -> Result<ProcessOutcome, ProcessError> {
    let mut command = restricted_command(program);
    command
        .args(arguments)
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(if capture_stdout { Stdio::piped() } else { Stdio::null() })
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|_| ProcessError::Unavailable)?;
    // The overall deadline starts here and covers the stdin write as well as the
    // wait: a child that never drains stdin combined with a large input would
    // otherwise block the parent forever once the pipe buffer fills. The write
    // runs on its own thread so the polling loop can kill an overdue child,
    // which breaks the pipe and unblocks the writer.
    let started = Instant::now();
    let writer = match input {
        Some(input) => {
            let input = input.to_vec();
            let mut stdin = child.stdin.take().ok_or(ProcessError::Input)?;
            Some(thread::spawn(move || {
                let result = stdin.write_all(&input);
                drop(stdin);
                result
            }))
        }
        None => None,
    };
    let stdout_reader = child.stdout.take().map(|mut stdout| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            stdout.read_to_end(&mut bytes).map(|_| bytes)
        })
    });
    let stderr_reader = child.stderr.take().map(|mut stderr| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            stderr.read_to_end(&mut bytes).map(|_| bytes)
        })
    });
    let status = wait_for_child(&mut child, started)?;
    if let Some(writer) = writer {
        match writer.join() {
            Ok(Ok(())) => {}
            _ => return Err(ProcessError::Input),
        }
    }
    let stdout = join_pipe(stdout_reader)?;
    let stderr = join_pipe(stderr_reader)?;
    Ok(ProcessOutcome { success: status.success(), stdout, stderr })
}

fn join_pipe(
    reader: Option<thread::JoinHandle<std::io::Result<Vec<u8>>>>,
) -> Result<Vec<u8>, ProcessError> {
    match reader {
        Some(reader) => reader
            .join()
            .map_err(|_| ProcessError::Output)?
            .map_err(|_| ProcessError::Output),
        None => Ok(Vec::new()),
    }
}

fn wait_for_child(
    child: &mut Child,
    started: Instant,
) -> Result<std::process::ExitStatus, ProcessError> {
    loop {
        if let Some(status) = child.try_wait().map_err(|_| ProcessError::Wait)? {
            return Ok(status);
        }
        if started.elapsed() >= PROCESS_DEADLINE {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ProcessError::TimedOut);
        }
        thread::sleep(PROCESS_POLL);
    }
}
