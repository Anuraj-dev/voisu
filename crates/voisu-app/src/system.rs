use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use voisu_core::{
    socket_path, ActiveCapture, AudioCapture, AudioChunk, BoundaryError, BoundaryFuture,
    BoundaryKind, CancelRegistry, CapturedAudio, Command as DaemonCommand, Credential,
    DeliveryAdapter, Provider,
    ProviderAuthenticator, ProviderStream, ReadinessCapability, ReadinessFinding,
    MergeResult, ReadinessInspector, ReadinessStatus, ReconciliationKind, ReconciliationModel,
    Request, Response, SecretStore, SourceTranscript, Transcript, TranscriptDecision,
    TranscriptDecisionPipeline, TranscriptProvider, TranscriptValidator, VersionEnvelope,
    PROTOCOL_VERSION,
};

const PROCESS_DEADLINE: Duration = Duration::from_secs(2);
pub const CAPTURE_FINALIZE_DEADLINE: Duration = PROCESS_DEADLINE;
pub const PROVIDER_COMPLETION_DEADLINE: Duration = Duration::from_secs(15);
pub const CLIPBOARD_DELIVERY_DEADLINE: Duration = PROCESS_DEADLINE;
/// Grace granted to the bounded capture/provider aborts that run when a
/// Recording fails or a partial start is rolled back.
pub const RECOVERY_ABORT_DEADLINE: Duration = PROCESS_DEADLINE;
pub const RECONCILIATION_DEADLINE: Duration = Duration::from_secs(3);
pub const PROCESSING_RESPONSE_DEADLINE: Duration = Duration::from_secs(
    CAPTURE_FINALIZE_DEADLINE.as_secs()
        + PROVIDER_COMPLETION_DEADLINE.as_secs()
        + CLIPBOARD_DELIVERY_DEADLINE.as_secs()
        + RECOVERY_ABORT_DEADLINE.as_secs()
        + RECONCILIATION_DEADLINE.as_secs() * 2
        + 1,
);
const PROCESS_POLL: Duration = Duration::from_millis(10);
const MAX_DAEMON_RESPONSE_BYTES: usize = 16 * 1024;
const MAX_RETAINED_STDERR_BYTES: usize = 4 * 1024;
const MAX_RETAINED_STDOUT_BYTES: usize = 64 * 1024;
const PROVIDER_PROCESS_DEADLINE: Duration = Duration::from_secs(14);
const RECONCILIATION_PROCESS_DEADLINE: Duration = Duration::from_secs(2);
const PCM_CHUNK_BYTES: usize = 3_200;
const MIN_RECORDING_BYTES: usize = PCM_CHUNK_BYTES;
const MAX_RECORDING_BYTES: usize = 16_000 * 2 * 60 * 5;
const GROQ_CHUNK_BYTES: usize = 16_000 * 2 * 30;
const GROQ_CHUNK_OVERLAP_BYTES: usize = 16_000;
const DEEPGRAM_CHUNK_BYTES: usize = 16_000 * 2;
const MAX_DEEPGRAM_IN_FLIGHT: usize = 3;

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
                // Reject before appending: a flooding peer must never force an
                // allocation beyond the response cap.
                if response.len() + read > MAX_DAEMON_RESPONSE_BYTES {
                    return Err(());
                }
                response.extend_from_slice(&buffer[..read]);
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
    for name in [
        "XDG_RUNTIME_DIR",
        "DBUS_SESSION_BUS_ADDRESS",
        "WAYLAND_DISPLAY",
        "XDG_SESSION_TYPE",
    ] {
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
    run_restricted_with_deadline(program, arguments, input, capture_stdout, PROCESS_DEADLINE, None)
}

fn run_restricted_with_deadline(
    program: &str,
    arguments: &[&str],
    input: Option<&[u8]>,
    capture_stdout: bool,
    deadline: Duration,
    cancel: Option<&CancelRegistry>,
) -> Result<ProcessOutcome, ProcessError> {
    // Fail fast without spawning when the operation is already cancelled.
    if cancel.is_some_and(CancelRegistry::is_cancelled) {
        return Err(ProcessError::TimedOut);
    }
    let started = Instant::now();
    let mut command = restricted_command(program);
    command
        .args(arguments)
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(if capture_stdout { Stdio::piped() } else { Stdio::null() })
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|_| ProcessError::Unavailable)?;
    // The whole-operation deadline starts before spawn and covers startup, the
    // stdin write, pipe drains, and wait. The write runs on its own thread so
    // the polling loop can kill an overdue child, which breaks the pipe and
    // unblocks the writer.
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
        thread::spawn(move || read_capped(&mut stdout, MAX_RETAINED_STDOUT_BYTES))
    });
    let stderr_reader = child.stderr.take().map(|mut stderr| {
        thread::spawn(move || read_capped(&mut stderr, MAX_RETAINED_STDERR_BYTES))
    });
    // Every helper thread join is bounded by the same Instant budget on every
    // path: a descendant of the child can inherit and hold the pipes open past
    // the child's own exit, which would otherwise block a bare join() forever
    // (or, on the error path, silently leave detached threads blocked).
    // Collect every helper-thread result FIRST, then decide the outcome: an
    // early return between joins would silently detach a later thread while it
    // may still be blocked on a descendant-held pipe.
    let status = wait_for_child(&mut child, started, deadline, cancel);
    let writer = writer.map(|handle| bounded_join(handle, started, &mut child, deadline));
    let stdout_joined = stdout_reader.map(|handle| bounded_join(handle, started, &mut child, deadline));
    let stderr_joined = stderr_reader.map(|handle| bounded_join(handle, started, &mut child, deadline));
    let stdout = pipe_bytes(stdout_joined)?;
    let stderr = pipe_bytes(stderr_joined)?;
    let status = status?;
    if let Some(writer) = writer {
        match writer {
            Ok(Ok(())) => {}
            Err(ProcessError::TimedOut) => return Err(ProcessError::TimedOut),
            _ => return Err(ProcessError::Input),
        }
    }
    Ok(ProcessOutcome { success: status.success(), stdout, stderr })
}

/// Joins a helper thread under the remaining process budget. On budget
/// exhaustion the overdue child is killed and the thread is deliberately
/// detached — it can never be forced to finish while a descendant holds the
/// pipe — and the caller receives the timeout error.
fn bounded_join<T: Send + 'static>(
    handle: thread::JoinHandle<T>,
    started: Instant,
    child: &mut Child,
    deadline: Duration,
) -> Result<T, ProcessError> {
    while !handle.is_finished() {
        if started.elapsed() >= deadline {
            let _ = child.kill();
            reap_briefly(child);
            drop(handle);
            return Err(ProcessError::TimedOut);
        }
        thread::sleep(PROCESS_POLL);
    }
    handle.join().map_err(|_| ProcessError::Output)
}

/// Best-effort reap of a killed child under a small extra budget so no zombie
/// is left behind; if it still has not been collected, give up rather than
/// block the caller further.
fn reap_briefly(child: &mut Child) {
    let reap_started = Instant::now();
    while reap_started.elapsed() < Duration::from_millis(250) {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => thread::sleep(PROCESS_POLL),
        }
    }
}

fn pipe_bytes(
    joined: Option<Result<std::io::Result<Vec<u8>>, ProcessError>>,
) -> Result<Vec<u8>, ProcessError> {
    match joined {
        Some(result) => result?.map_err(|_| ProcessError::Output),
        None => Ok(Vec::new()),
    }
}

/// Drains a pipe to EOF so the child never blocks on a full buffer, but
/// retains only the first `cap` bytes: a noisy child cannot force unbounded
/// memory growth inside the deadline window.
fn read_capped(source: &mut impl Read, cap: usize) -> std::io::Result<Vec<u8>> {
    let mut retained = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        match source.read(&mut buffer) {
            Ok(0) => return Ok(retained),
            Ok(read) => {
                let room = cap.saturating_sub(retained.len());
                retained.extend_from_slice(&buffer[..read.min(room)]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

fn wait_for_child(
    child: &mut Child,
    started: Instant,
    deadline: Duration,
    cancel: Option<&CancelRegistry>,
) -> Result<std::process::ExitStatus, ProcessError> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(_) => {
                // The child may still be live even though its status cannot be
                // read; kill and best-effort reap before surfacing the error.
                let _ = child.kill();
                reap_briefly(child);
                return Err(ProcessError::Wait);
            }
        }
        // Cancellation is observed by the loop that owns the Child handle:
        // killing through the handle is pid-reuse-safe because this loop is
        // also the only reaper. Latency is at most one poll tick.
        if cancel.is_some_and(CancelRegistry::is_cancelled)
            || started.elapsed() >= deadline
        {
            let _ = child.kill();
            reap_briefly(child);
            return Err(ProcessError::TimedOut);
        }
        thread::sleep(PROCESS_POLL);
    }
}

pub struct PipeWireCapture;

struct CaptureReaderState {
    chunks: VecDeque<AudioChunk>,
    received_bytes: usize,
    eof: bool,
    error: Option<String>,
}

impl AudioCapture for PipeWireCapture {
    fn begin(&mut self, _recording_id: u64) -> Result<Box<dyn ActiveCapture>, BoundaryError> {
        let mut command = restricted_command("pw-record");
        command.args([
            "--raw",
            "--rate",
            "16000",
            "--channels",
            "1",
            "--format",
            "s16",
        ]);
        if let Some(target) = std::env::var_os("VOISU_PIPEWIRE_TARGET") {
            command.arg("--target").arg(target);
        }
        command
            .arg("-")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(target_os = "linux")]
        // SAFETY: this hook only invokes the async-signal-safe `prctl` syscall
        // between fork and exec; it does not allocate or touch shared state.
        unsafe {
            command.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let mut child = command.spawn().map_err(|_| {
            BoundaryError::new(BoundaryKind::Capture, "pw-record unavailable")
        })?;
        let mut stdout = child.stdout.take().ok_or_else(|| {
            BoundaryError::new(BoundaryKind::Capture, "pw-record stdout unavailable")
        })?;
        let mut stderr = child.stderr.take().ok_or_else(|| {
            BoundaryError::new(BoundaryKind::Capture, "pw-record stderr unavailable")
        })?;
        let state = Arc::new(Mutex::new(CaptureReaderState {
            chunks: VecDeque::new(),
            received_bytes: 0,
            eof: false,
            error: None,
        }));
        let reader_state = Arc::clone(&state);
        let reader = thread::spawn(move || {
            let mut buffer = vec![0_u8; PCM_CHUNK_BYTES];
            loop {
                match stdout.read(&mut buffer) {
                    Ok(0) => {
                        reader_state.lock().unwrap().eof = true;
                        return;
                    }
                    Ok(read) => {
                        let mut state = reader_state.lock().unwrap();
                        state.received_bytes = state.received_bytes.saturating_add(read);
                        if state.received_bytes <= MAX_RECORDING_BYTES {
                            state.chunks.push_back(AudioChunk(buffer[..read].to_vec()));
                        } else if state.error.is_none() {
                            state.error = Some("Recording exceeded the bounded audio buffer".to_owned());
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => {
                        let mut state = reader_state.lock().unwrap();
                        state.error = Some("pw-record audio read failed".to_owned());
                        state.eof = true;
                        return;
                    }
                }
            }
        });
        let stderr_reader = thread::spawn(move || {
            read_capped(&mut stderr, MAX_RETAINED_STDERR_BYTES).unwrap_or_default()
        });
        let deadline = std::env::var("VOISU_RECORDING_DEADLINE_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_millis)
            .filter(|value| !value.is_zero())
            .unwrap_or(Duration::from_secs(60));
        Ok(Box::new(PipeWireActiveCapture {
            child: Some(child),
            state,
            reader: Some(reader),
            stderr_reader: Some(stderr_reader),
            pcm: Vec::new(),
            started: Instant::now(),
            deadline,
        }))
    }
}

struct PipeWireActiveCapture {
    child: Option<Child>,
    state: Arc<Mutex<CaptureReaderState>>,
    reader: Option<thread::JoinHandle<()>>,
    stderr_reader: Option<thread::JoinHandle<Vec<u8>>>,
    pcm: Vec<u8>,
    started: Instant,
    deadline: Duration,
}

impl PipeWireActiveCapture {
    fn drain_chunks(&mut self) {
        let mut state = self.state.lock().unwrap();
        while let Some(chunk) = state.chunks.pop_front() {
            self.pcm.extend_from_slice(&chunk.0);
        }
    }

    fn stop_child(&mut self, graceful: bool) -> Result<Vec<u8>, BoundaryError> {
        let mut child = self.child.take().ok_or_else(|| {
            BoundaryError::new(BoundaryKind::Capture, "pw-record already finalized")
        })?;
        if graceful {
            if let Some(pid) = child.id().try_into().ok() {
                unsafe {
                    libc::kill(pid, libc::SIGINT);
                }
            }
        } else {
            let _ = child.kill();
        }
        let stopped = Instant::now();
        let status = wait_for_child(&mut child, stopped, PROCESS_DEADLINE, None);
        let reader = self
            .reader
            .take()
            .map(|handle| bounded_join(handle, stopped, &mut child, PROCESS_DEADLINE));
        let stderr = self
            .stderr_reader
            .take()
            .map(|handle| bounded_join(handle, stopped, &mut child, PROCESS_DEADLINE));
        if !matches!(reader, None | Some(Ok(()))) {
            return Err(BoundaryError::new(
                BoundaryKind::Capture,
                "pw-record audio drain failed",
            ));
        }
        let stderr = match stderr {
            Some(Ok(bytes)) => bytes,
            None => Vec::new(),
            Some(Err(_)) => {
                return Err(BoundaryError::new(
                    BoundaryKind::Capture,
                    "pw-record diagnostic drain failed",
                ));
            }
        };
        let status = status.map_err(|error| capture_process_error(error, &stderr))?;
        let expected_signal = if graceful { libc::SIGINT } else { libc::SIGKILL };
        if !status.success() && status.signal() != Some(expected_signal) {
            return Err(BoundaryError::new(
                BoundaryKind::Capture,
                process_diagnostic("pw-record failed", &stderr),
            ));
        }
        Ok(stderr)
    }

    fn validate_audio(&self) -> Result<(), BoundaryError> {
        if self.pcm.is_empty() {
            return Err(BoundaryError::new(
                BoundaryKind::EmptyRecording,
                "pw-record returned no audio frames",
            ));
        }
        if self.pcm.len() < MIN_RECORDING_BYTES {
            return Err(BoundaryError::new(
                BoundaryKind::TooShortRecording,
                format!("Recording contained {} PCM bytes", self.pcm.len()),
            ));
        }
        let audible = self.pcm.chunks_exact(2).any(|sample| {
            i16::from_le_bytes([sample[0], sample[1]]).unsigned_abs() > 32
        });
        if !audible {
            return Err(BoundaryError::new(
                BoundaryKind::SilentRecording,
                "Recording peak amplitude did not exceed the silence floor",
            ));
        }
        Ok(())
    }
}

impl ActiveCapture for PipeWireActiveCapture {
    fn next_chunk(&mut self) -> BoundaryFuture<'_, Option<AudioChunk>> {
        Box::pin(async move {
            loop {
                if self.started.elapsed() >= self.deadline {
                    return Err(BoundaryError::new(
                        BoundaryKind::RecordingDeadline,
                        "configured Recording Deadline elapsed",
                    ));
                }
                let next = {
                    let mut state = self.state.lock().unwrap();
                    if let Some(error) = state.error.clone() {
                        return Err(BoundaryError::new(BoundaryKind::Capture, error));
                    }
                    (state.chunks.pop_front(), state.eof)
                };
                match next {
                    (Some(chunk), _) => {
                        self.pcm.extend_from_slice(&chunk.0);
                        return Ok(Some(chunk));
                    }
                    (None, true) => return Ok(None),
                    (None, false) => tokio::time::sleep(PROCESS_POLL).await,
                }
            }
        })
    }

    fn finish(&mut self) -> BoundaryFuture<'_, CapturedAudio> {
        Box::pin(async move {
            self.stop_child(true)?;
            self.drain_chunks();
            if let Some(error) = self.state.lock().unwrap().error.clone() {
                return Err(BoundaryError::new(BoundaryKind::Capture, error));
            }
            self.validate_audio()?;
            Ok(CapturedAudio::new(std::mem::take(&mut self.pcm)))
        })
    }

    fn abort(mut self: Box<Self>) -> BoundaryFuture<'static, ()> {
        Box::pin(async move {
            self.stop_child(false)?;
            Ok(())
        })
    }
}

impl Drop for PipeWireActiveCapture {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            reap_briefly(&mut child);
        }
    }
}

fn capture_process_error(error: ProcessError, stderr: &[u8]) -> BoundaryError {
    let detail = match error {
        ProcessError::Unavailable => "pw-record unavailable".to_owned(),
        ProcessError::TimedOut => "pw-record cleanup deadline elapsed".to_owned(),
        ProcessError::Input | ProcessError::Wait | ProcessError::Output => {
            process_diagnostic("pw-record execution failed", stderr)
        }
    };
    BoundaryError::new(BoundaryKind::Capture, detail)
}

fn process_diagnostic(prefix: &str, stderr: &[u8]) -> String {
    let detail = String::from_utf8_lossy(stderr);
    let detail = detail.trim();
    if detail.is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix}: {detail}")
    }
}

pub struct GroqProvider;

impl TranscriptProvider for GroqProvider {
    fn start(&mut self, _recording_id: u64) -> Result<Box<dyn ProviderStream>, BoundaryError> {
        let credential = SecretStore::load(&mut SecretToolStore, Provider::Groq)?;
        let endpoint = std::env::var("VOISU_GROQ_TRANSCRIPTION_URL")
            .unwrap_or_else(|_| "https://api.groq.com/openai/v1/audio/transcriptions".to_owned());
        if !provider_endpoint_is_secure(&endpoint) {
            return Err(BoundaryError::new(
                BoundaryKind::Provider,
                "Groq transcription endpoint must use HTTPS except on loopback",
            ));
        }
        Ok(Box::new(GroqStream {
            credential,
            endpoint,
            buffer: Vec::new(),
            streamed_bytes: 0,
            chunks: VecDeque::new(),
            cancel: CancelRegistry::new(),
        }))
    }
}

fn provider_endpoint_is_secure(endpoint: &str) -> bool {
    if endpoint.contains(['\n', '\r']) {
        return false;
    }
    if endpoint.starts_with("https://") {
        return true;
    }
    let Some(remainder) = endpoint.strip_prefix("http://") else {
        return false;
    };
    let authority = remainder.split('/').next().unwrap_or_default().to_ascii_lowercase();
    authority == "localhost"
        || authority.starts_with("localhost:")
        || authority == "127.0.0.1"
        || authority.starts_with("127.0.0.1:")
        || authority == "[::1]"
        || authority.starts_with("[::1]:")
}

struct GroqStream {
    credential: Credential,
    endpoint: String,
    buffer: Vec<u8>,
    streamed_bytes: usize,
    chunks: VecDeque<tokio::task::JoinHandle<Result<String, BoundaryError>>>,
    /// Per-Recording cancellation flag observed by each in-flight curl
    /// request's owning bounded wait. Because each Recording gets its own
    /// stream and flag, cancelling one Recording can never touch the next
    /// one's requests, and stale results die with their aborted stream.
    cancel: Arc<CancelRegistry>,
}

impl Drop for GroqStream {
    fn drop(&mut self) {
        self.cancel.cancel();
        for chunk in self.chunks.drain(..) {
            chunk.abort();
        }
    }
}

impl ProviderStream for GroqStream {
    fn provider(&self) -> Provider {
        Provider::Groq
    }

    fn send_audio(&mut self, chunk: AudioChunk) -> BoundaryFuture<'_, ()> {
        Box::pin(async move {
            self.streamed_bytes = self.streamed_bytes.saturating_add(chunk.0.len());
            self.buffer.extend_from_slice(&chunk.0);
            while self.buffer.len() >= GROQ_CHUNK_BYTES {
                let pcm = self.buffer[..GROQ_CHUNK_BYTES].to_vec();
                self.buffer = self.buffer
                    [GROQ_CHUNK_BYTES - GROQ_CHUNK_OVERLAP_BYTES..]
                    .to_vec();
                let credential = self.credential.clone();
                let endpoint = self.endpoint.clone();
                let cancel = Arc::clone(&self.cancel);
                self.chunks.push_back(tokio::spawn(async move {
                    ProviderHttpClient
                        .transcribe_groq_chunk(credential, endpoint, pcm, cancel)
                        .await
                }));
            }
            Ok(())
        })
    }

    fn abort(mut self: Box<Self>) -> BoundaryFuture<'static, ()> {
        Box::pin(async move {
            // Cancel the in-flight curl children first: each owning bounded
            // wait observes the flag within one poll tick and kills through
            // its own Child handle. Aborting the tasks alone would detach
            // already-running blocking requests, letting work from the failed
            // Recording overlap the next one.
            self.cancel.cancel();
            for chunk in self.chunks.drain(..) {
                let _ = chunk.await;
            }
            Ok(())
        })
    }

    fn complete(&mut self, audio: CapturedAudio) -> BoundaryFuture<'_, SourceTranscript> {
        Box::pin(async move {
            let pcm = audio.pcm_s16le_mono_16khz();
            if self.streamed_bytes > pcm.len() {
                return Err(BoundaryError::new(
                    BoundaryKind::Provider,
                    "Groq stream exceeded the finalized Recording",
                ));
            }
            self.buffer.extend_from_slice(&pcm[self.streamed_bytes..]);
            let mut transcripts = Vec::new();
            while let Some(chunk) = self.chunks.front_mut() {
                let transcript = chunk.await.map_err(|_| {
                    BoundaryError::new(BoundaryKind::Provider, "Groq chunk task failed")
                })??;
                self.chunks.pop_front();
                transcripts.push(transcript);
            }
            let needs_final_chunk = transcripts.is_empty()
                || self.buffer.len() > GROQ_CHUNK_OVERLAP_BYTES;
            if needs_final_chunk {
                transcripts.push(
                    ProviderHttpClient
                        .transcribe_groq_chunk(
                            self.credential.clone(),
                            self.endpoint.clone(),
                            std::mem::take(&mut self.buffer),
                            Arc::clone(&self.cancel),
                        )
                        .await?,
                );
            }
            let text = merge_chunk_transcripts(transcripts);
            Ok(SourceTranscript {
                provider: Provider::Groq,
                text,
            })
        })
    }
}

pub struct DeepgramProvider;

impl TranscriptProvider for DeepgramProvider {
    fn start(&mut self, _recording_id: u64) -> Result<Box<dyn ProviderStream>, BoundaryError> {
        let credential = SecretStore::load(&mut SecretToolStore, Provider::Deepgram)?;
        let endpoint = std::env::var("VOISU_DEEPGRAM_TRANSCRIPTION_URL").unwrap_or_else(|_| {
            "https://api.deepgram.com/v1/listen?model=nova-3&encoding=linear16&sample_rate=16000&channels=1"
                .to_owned()
        });
        if !provider_endpoint_is_secure(&endpoint) {
            return Err(BoundaryError::new(
                BoundaryKind::Provider,
                "Deepgram transcription endpoint must use HTTPS except on loopback",
            ));
        }
        Ok(Box::new(DeepgramStream {
            credential,
            endpoint,
            buffer: Vec::new(),
            streamed_bytes: 0,
            chunks: VecDeque::new(),
            permits: Arc::new(tokio::sync::Semaphore::new(MAX_DEEPGRAM_IN_FLIGHT)),
            cancel: CancelRegistry::new(),
        }))
    }
}

struct DeepgramStream {
    credential: Credential,
    endpoint: String,
    buffer: Vec<u8>,
    streamed_bytes: usize,
    chunks: VecDeque<tokio::task::JoinHandle<Result<String, BoundaryError>>>,
    permits: Arc<tokio::sync::Semaphore>,
    cancel: Arc<CancelRegistry>,
}

impl Drop for DeepgramStream {
    fn drop(&mut self) {
        self.cancel.cancel();
        for chunk in self.chunks.drain(..) {
            chunk.abort();
        }
    }
}

impl ProviderStream for DeepgramStream {
    fn provider(&self) -> Provider {
        Provider::Deepgram
    }

    fn send_audio(&mut self, chunk: AudioChunk) -> BoundaryFuture<'_, ()> {
        Box::pin(async move {
            self.streamed_bytes = self.streamed_bytes.saturating_add(chunk.0.len());
            self.buffer.extend_from_slice(&chunk.0);
            while self.buffer.len() >= DEEPGRAM_CHUNK_BYTES {
                let pcm = self.buffer.drain(..DEEPGRAM_CHUNK_BYTES).collect();
                let credential = self.credential.clone();
                let endpoint = self.endpoint.clone();
                let cancel = Arc::clone(&self.cancel);
                let permits = Arc::clone(&self.permits);
                self.chunks.push_back(tokio::spawn(async move {
                    let _permit = permits.acquire_owned().await.map_err(|_| {
                        BoundaryError::new(BoundaryKind::Provider, "Deepgram request queue closed")
                    })?;
                    ProviderHttpClient
                        .transcribe_deepgram_chunk(credential, endpoint, pcm, cancel)
                        .await
                }));
            }
            Ok(())
        })
    }

    fn abort(mut self: Box<Self>) -> BoundaryFuture<'static, ()> {
        Box::pin(async move {
            self.cancel.cancel();
            for chunk in self.chunks.drain(..) {
                let _ = chunk.await;
            }
            Ok(())
        })
    }

    fn complete(&mut self, audio: CapturedAudio) -> BoundaryFuture<'_, SourceTranscript> {
        Box::pin(async move {
            let pcm = audio.pcm_s16le_mono_16khz();
            if self.streamed_bytes > pcm.len() {
                return Err(BoundaryError::new(
                    BoundaryKind::Provider,
                    "Deepgram stream exceeded the finalized Recording",
                ));
            }
            self.buffer.extend_from_slice(&pcm[self.streamed_bytes..]);
            if !self.buffer.is_empty() || self.chunks.is_empty() {
                let credential = self.credential.clone();
                let endpoint = self.endpoint.clone();
                let tail = std::mem::take(&mut self.buffer);
                let cancel = Arc::clone(&self.cancel);
                let permits = Arc::clone(&self.permits);
                self.chunks.push_back(tokio::spawn(async move {
                    let _permit = permits.acquire_owned().await.map_err(|_| {
                        BoundaryError::new(BoundaryKind::Provider, "Deepgram request queue closed")
                    })?;
                    ProviderHttpClient
                        .transcribe_deepgram_chunk(credential, endpoint, tail, cancel)
                        .await
                }));
            }
            let mut transcripts = Vec::new();
            // Await the in-flight chunk WITHOUT removing it from `self.chunks`.
            // If this completion future is dropped mid-await (e.g. the Provider
            // Deadline elapses and the coordinator moves to `abort()`), the
            // chunk must still be in the deque so the gated `abort()` awaits and
            // reaps its curl child before Idle is observable. Popping it here
            // would detach that reap and race the Idle transition.
            while let Some(chunk) = self.chunks.front_mut() {
                match await_deepgram_chunk(chunk).await {
                    Ok(transcript) => {
                        self.chunks.pop_front();
                        transcripts.push(transcript);
                    }
                    Err(error) => {
                        // Cancel the siblings so their curl children are killed,
                        // then drop the already-awaited front handle (re-awaiting
                        // a completed JoinHandle panics) and await the rest so
                        // their reaps complete before this error surfaces. Each
                        // sibling is awaited through `front_mut()` and popped only
                        // AFTER its await completes: if the Provider Deadline drops
                        // this future mid-cleanup, the unfinished handles are still
                        // in the deque for the gated `abort()` to own and reap —
                        // draining first would detach them on drop.
                        self.cancel.cancel();
                        self.chunks.pop_front();
                        while let Some(chunk) = self.chunks.front_mut() {
                            let _ = chunk.await;
                            self.chunks.pop_front();
                        }
                        return Err(error);
                    }
                }
            }
            Ok(SourceTranscript {
                provider: Provider::Deepgram,
                text: concatenate_chunk_transcripts(transcripts),
            })
        })
    }
}

async fn await_deepgram_chunk(
    chunk: &mut tokio::task::JoinHandle<Result<String, BoundaryError>>,
) -> Result<String, BoundaryError> {
    chunk.await.map_err(|_| {
        BoundaryError::new(BoundaryKind::Provider, "Deepgram chunk task failed")
    })?
}

pub struct MergeResultValidator {
    pipeline: TranscriptDecisionPipeline<GroqReconciliationModel>,
}

impl MergeResultValidator {
    pub fn new() -> Self {
        Self {
            pipeline: TranscriptDecisionPipeline::new(
                GroqReconciliationModel,
                RECONCILIATION_DEADLINE,
            ),
        }
    }
}

impl Default for MergeResultValidator {
    fn default() -> Self {
        Self::new()
    }
}

impl TranscriptValidator for MergeResultValidator {
    fn validate(
        &mut self,
        sources: Vec<SourceTranscript>,
    ) -> BoundaryFuture<'_, TranscriptDecision> {
        self.pipeline.validate(sources)
    }
}

struct GroqReconciliationModel;

impl ReconciliationModel for GroqReconciliationModel {
    fn request(
        &mut self,
        kind: ReconciliationKind,
        sources: Vec<SourceTranscript>,
        candidate: Option<MergeResult>,
        cancel: Arc<CancelRegistry>,
    ) -> BoundaryFuture<'_, MergeResult> {
        Box::pin(async move {
            // The whole operation — including the potentially slow synchronous
            // Secret Service lookup — runs inside ONE owned blocking task, so
            // it never blocks the async thread and the pipeline can cancel it
            // as a unit. curl observes the cancel flag through its bounded
            // wait: on cancellation the child is killed and reaped by the same
            // loop that owns its handle, and this future completes only after
            // that cleanup, keeping the reap ordered before any fallback
            // becomes observable. The post-lookup check guarantees no curl is
            // spawned once the deadline has already cancelled the request.
            tokio::task::spawn_blocking(move || {
                let credential = SecretStore::load(&mut SecretToolStore, Provider::Groq)?;
                if cancel.is_cancelled() {
                    return Err(BoundaryError::new(
                        BoundaryKind::Validation,
                        "reconciliation request cancelled",
                    ));
                }
                request_groq_reconciliation(credential, kind, sources, candidate, &cancel)
            })
            .await
            .map_err(|_| {
                BoundaryError::new(BoundaryKind::Validation, "reconciliation request task failed")
            })?
        })
    }
}

fn request_groq_reconciliation(
    credential: Credential,
    kind: ReconciliationKind,
    sources: Vec<SourceTranscript>,
    candidate: Option<MergeResult>,
    cancel: &CancelRegistry,
) -> Result<MergeResult, BoundaryError> {
    let endpoint = std::env::var("VOISU_GROQ_RECONCILIATION_URL")
        .unwrap_or_else(|_| "https://api.groq.com/openai/v1/chat/completions".to_owned());
    if !provider_endpoint_is_secure(&endpoint) {
        return Err(BoundaryError::new(
            BoundaryKind::Validation,
            "Groq reconciliation endpoint must use HTTPS except on loopback",
        ));
    }
    let model = std::env::var("VOISU_GROQ_RECONCILIATION_MODEL")
        .unwrap_or_else(|_| "llama-3.3-70b-versatile".to_owned());
    if model.trim().is_empty() || model.contains(['\n', '\r']) {
        return Err(BoundaryError::new(
            BoundaryKind::Validation,
            "invalid Groq reconciliation model",
        ));
    }
    let source_text = sources
        .iter()
        .map(|source| format!("{}: {}", source.provider.cli_label(), source.text))
        .collect::<Vec<_>>()
        .join("\n");
    let task = match (kind, candidate) {
        (ReconciliationKind::Reconcile, _) => format!(
            "Reconcile these Source Transcripts. Return only the faithful final Transcript, with no labels, explanation, or added content.\n{source_text}"
        ),
        (ReconciliationKind::Repair, Some(candidate)) => format!(
            "Repair this unsafe candidate using only the Source Transcripts. Return only the faithful final Transcript, with no labels, explanation, or added content.\nCandidate: {}\n{source_text}",
            candidate.0
        ),
        (ReconciliationKind::Repair, None) => {
            return Err(BoundaryError::new(
                BoundaryKind::Validation,
                "reconciliation recovery omitted its candidate",
            ));
        }
    };
    let body = serde_json::json!({
        "model": model,
        "temperature": 0,
        "messages": [
            {
                "role": "system",
                "content": "You are Voisu's Transcript reconciliation model. Preserve spoken meaning and never add commentary, prompt text, or facts."
            },
            { "role": "user", "content": task }
        ]
    })
    .to_string();
    let config = format!(
        "url = \"{}\"\nheader = \"Authorization: Bearer {}\"\nheader = \"Content-Type: application/json\"\ndata = \"{}\"\n",
        curl_config_escape(&endpoint),
        curl_config_escape(credential.expose_to_boundary()),
        curl_config_escape(&body),
    );
    let outcome = run_restricted_with_deadline(
        "curl",
        &[
            "-q",
            "--config",
            "-",
            "--fail",
            "--silent",
            "--show-error",
            "--max-time",
            "2",
        ],
        Some(config.as_bytes()),
        true,
        RECONCILIATION_PROCESS_DEADLINE,
        Some(cancel),
    )
    .map_err(|error| match error {
        ProcessError::TimedOut => {
            BoundaryError::new(BoundaryKind::Validation, "reconciliation request deadline elapsed")
        }
        _ => BoundaryError::new(
            BoundaryKind::Validation,
            "Groq reconciliation request unavailable or failed",
        ),
    })?;
    if !outcome.success {
        return Err(BoundaryError::new(
            BoundaryKind::Validation,
            "Groq rejected the reconciliation request",
        ));
    }
    let response: serde_json::Value = serde_json::from_slice(&outcome.stdout).map_err(|_| {
        BoundaryError::new(BoundaryKind::Validation, "Groq reconciliation returned malformed JSON")
    })?;
    response
        .pointer("/choices/0/message/content")
        .and_then(|text| text.as_str())
        .map(|text| MergeResult(text.to_owned()))
        .ok_or_else(|| {
            BoundaryError::new(BoundaryKind::Validation, "Groq reconciliation omitted text")
        })
}

pub struct ClipboardDelivery;

impl DeliveryAdapter for ClipboardDelivery {
    fn deliver(&mut self, transcript: Transcript) -> BoundaryFuture<'_, ()> {
        Box::pin(async move {
            let result = tokio::task::spawn_blocking(move || {
                run_restricted("wl-copy", &[], Some(transcript.0.as_bytes()), false)
            })
            .await
            .map_err(|_| {
                BoundaryError::new(BoundaryKind::Delivery, "wl-copy task failed")
            })?;
            match result {
                Ok(outcome) if outcome.success => Ok(()),
                Ok(_outcome) => Err(BoundaryError::new(
                    BoundaryKind::Delivery,
                    "wl-copy rejected the Transcript",
                )),
                Err(ProcessError::TimedOut) => Err(BoundaryError::new(
                    BoundaryKind::Delivery,
                    "wl-copy deadline elapsed",
                )),
                Err(_) => Err(BoundaryError::new(
                    BoundaryKind::Delivery,
                    "wl-copy unavailable or failed",
                )),
            }
        })
    }
}

impl ProviderHttpClient {
    async fn transcribe_deepgram_chunk(
        &self,
        credential: Credential,
        endpoint: String,
        pcm: Vec<u8>,
        cancel: Arc<CancelRegistry>,
    ) -> Result<String, BoundaryError> {
        tokio::task::spawn_blocking(move || {
            request_deepgram_chunk(credential, endpoint, pcm, &cancel)
        })
        .await
        .map_err(|_| BoundaryError::new(BoundaryKind::Provider, "Deepgram request task failed"))?
    }

    async fn transcribe_groq_chunk(
        &self,
        credential: Credential,
        endpoint: String,
        pcm: Vec<u8>,
        cancel: Arc<CancelRegistry>,
    ) -> Result<String, BoundaryError> {
        tokio::task::spawn_blocking(move || request_groq_chunk(credential, endpoint, pcm, &cancel))
            .await
            .map_err(|_| BoundaryError::new(BoundaryKind::Provider, "Groq request task failed"))?
    }
}

fn request_deepgram_chunk(
    credential: Credential,
    endpoint: String,
    pcm: Vec<u8>,
    cancel: &CancelRegistry,
) -> Result<String, BoundaryError> {
    let mut file = tempfile::Builder::new()
        .prefix("voisu-deepgram-")
        .suffix(".pcm")
        .tempfile()
        .map_err(|_| BoundaryError::new(BoundaryKind::Provider, "temporary audio file unavailable"))?;
    file.write_all(&pcm)
        .and_then(|()| file.flush())
        .map_err(|_| BoundaryError::new(BoundaryKind::Provider, "temporary audio write failed"))?;
    let endpoint = curl_config_escape(&endpoint);
    let credential = curl_config_escape(credential.expose_to_boundary());
    let path = curl_config_escape(&file.path().to_string_lossy());
    let config = format!(
        "url = \"{endpoint}\"\nheader = \"Authorization: Token {credential}\"\nheader = \"Content-Type: audio/raw\"\ndata-binary = \"@{path}\"\n"
    );
    let outcome = run_restricted_with_deadline(
        "curl",
        &[
            "-q",
            "--config",
            "-",
            "--fail",
            "--silent",
            "--show-error",
            "--max-time",
            "15",
        ],
        Some(config.as_bytes()),
        true,
        PROVIDER_PROCESS_DEADLINE,
        Some(cancel),
    )
    .map_err(|error| match error {
        ProcessError::TimedOut => {
            BoundaryError::new(BoundaryKind::Provider, "Deepgram Provider Deadline elapsed")
        }
        _ => BoundaryError::new(
            BoundaryKind::Provider,
            "Deepgram request unavailable or failed",
        ),
    })?;
    if !outcome.success {
        return Err(BoundaryError::new(
            BoundaryKind::Provider,
            "Deepgram rejected the audio request",
        ));
    }
    let response: serde_json::Value = serde_json::from_slice(&outcome.stdout).map_err(|_| {
        BoundaryError::new(BoundaryKind::Provider, "Deepgram returned malformed JSON")
    })?;
    response
        .pointer("/results/channels/0/alternatives/0/transcript")
        .and_then(|text| text.as_str())
        .map(str::to_owned)
        .ok_or_else(|| {
            BoundaryError::new(BoundaryKind::Provider, "Deepgram response omitted text")
        })
}

fn request_groq_chunk(
    credential: Credential,
    endpoint: String,
    pcm: Vec<u8>,
    cancel: &CancelRegistry,
) -> Result<String, BoundaryError> {
    let mut file = tempfile::Builder::new()
        .prefix("voisu-recording-")
        .suffix(".wav")
        .tempfile()
        .map_err(|_| BoundaryError::new(BoundaryKind::Provider, "temporary audio file unavailable"))?;
    let wav = wav_from_pcm(&pcm)?;
    file.write_all(&wav)
        .and_then(|()| file.flush())
        .map_err(|_| BoundaryError::new(BoundaryKind::Provider, "temporary audio write failed"))?;
    let endpoint = curl_config_escape(&endpoint);
    let credential = curl_config_escape(credential.expose_to_boundary());
    let path = curl_config_escape(&file.path().to_string_lossy());
    let model = std::env::var("VOISU_GROQ_MODEL")
        .unwrap_or_else(|_| "whisper-large-v3-turbo".to_owned());
    if model.is_empty() || model.contains(['\n', '\r']) {
        return Err(BoundaryError::new(BoundaryKind::Provider, "invalid Groq model"));
    }
    let model = curl_config_escape(&model);
    let config = format!(
        "url = \"{endpoint}\"\nheader = \"Authorization: Bearer {credential}\"\nform = \"file=@{path};filename=recording.wav;type=audio/wav\"\nform = \"model={model}\"\nform = \"response_format=json\"\n"
    );
    let outcome = run_restricted_with_deadline(
        "curl",
        &[
            "-q",
            "--config",
            "-",
            "--fail",
            "--silent",
            "--show-error",
            "--max-time",
            "15",
        ],
        Some(config.as_bytes()),
        true,
        PROVIDER_PROCESS_DEADLINE,
        Some(cancel),
    )
    .map_err(|error| match error {
        ProcessError::TimedOut => {
            BoundaryError::new(BoundaryKind::Provider, "Groq Provider Deadline elapsed")
        }
        _ => BoundaryError::new(BoundaryKind::Provider, "Groq request unavailable or failed"),
    })?;
    if !outcome.success {
        return Err(BoundaryError::new(
            BoundaryKind::Provider,
            "Groq rejected the audio request",
        ));
    }
    let response: serde_json::Value = serde_json::from_slice(&outcome.stdout).map_err(|_| {
        BoundaryError::new(BoundaryKind::Provider, "Groq returned malformed JSON")
    })?;
    response
        .get("text")
        .and_then(|text| text.as_str())
        .map(str::to_owned)
        .ok_or_else(|| BoundaryError::new(BoundaryKind::Provider, "Groq response omitted text"))
}

fn merge_chunk_transcripts(transcripts: Vec<String>) -> String {
    let mut merged: Vec<String> = Vec::new();
    for transcript in transcripts {
        let words: Vec<String> = transcript
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        let overlap = (1..=merged.len().min(words.len()).min(24))
            .rev()
            .find(|count| merged[merged.len() - count..] == words[..*count])
            .unwrap_or(0);
        merged.extend(words.into_iter().skip(overlap));
    }
    merged.join(" ")
}

fn concatenate_chunk_transcripts(transcripts: Vec<String>) -> String {
    transcripts
        .into_iter()
        .flat_map(|transcript| {
            transcript
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn wav_from_pcm(pcm: &[u8]) -> Result<Vec<u8>, BoundaryError> {
    let data_len = u32::try_from(pcm.len()).map_err(|_| {
        BoundaryError::new(BoundaryKind::Provider, "Recording is too large for WAV")
    })?;
    let riff_len = data_len.checked_add(36).ok_or_else(|| {
        BoundaryError::new(BoundaryKind::Provider, "Recording WAV length overflow")
    })?;
    let mut wav = Vec::with_capacity(pcm.len() + 44);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&riff_len.to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&16_000_u32.to_le_bytes());
    wav.extend_from_slice(&32_000_u32.to_le_bytes());
    wav.extend_from_slice(&2_u16.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend_from_slice(pcm);
    Ok(wav)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_set_mid_wait_kills_the_owned_child_within_the_poll_bound() {
        let cancel = CancelRegistry::new();
        let registry = Arc::clone(&cancel);
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            registry.cancel();
        });
        let started = Instant::now();
        let result = run_restricted_with_deadline(
            "sleep",
            &["5"],
            None,
            false,
            Duration::from_secs(4),
            Some(&cancel),
        );
        canceller.join().unwrap();
        assert!(matches!(result, Err(ProcessError::TimedOut)));
        assert!(
            started.elapsed() < Duration::from_millis(600),
            "a mid-wait cancel must kill within the poll bound, elapsed {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn already_cancelled_operations_fail_fast_without_spawning() {
        let cancel = CancelRegistry::new();
        cancel.cancel();
        let started = Instant::now();
        let result = run_restricted_with_deadline(
            "sleep",
            &["5"],
            None,
            false,
            Duration::from_secs(4),
            Some(&cancel),
        );
        assert!(matches!(result, Err(ProcessError::TimedOut)));
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "an already-cancelled operation must not spawn, elapsed {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn non_overlapping_deepgram_chunks_keep_a_repeated_boundary_word() {
        assert_eq!(
            concatenate_chunk_transcripts(vec!["that was very".to_owned(), "very good".to_owned()]),
            "that was very very good"
        );
    }
}
