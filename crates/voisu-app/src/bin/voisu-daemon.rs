use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot, Semaphore};
use tokio::time::timeout;
use voisu_core::{
    ActiveCapture, AudioCapture, AudioChunk, BoundaryError, BoundaryFuture, BoundaryKind,
    CapturedAudio, Command, DaemonState, DeliveryAdapter, LifecycleEvidence, LifecycleStage,
    PROTOCOL_VERSION, Provider, ProviderCoordinator, ProviderStream, ProviderStreams, Request,
    Response, SourceTranscript, Transcript, TranscriptProvider, TranscriptValidator,
    VersionEnvelope, socket_path,
};

const MAX_FRAME_BYTES: u64 = 16 * 1024;
const IO_DEADLINE: Duration = Duration::from_secs(2);
const MAX_CONNECTIONS: usize = 32;
const PROVIDER_DEADLINE: Duration = Duration::from_secs(2);

#[tokio::main]
async fn main() {
    if let Err(message) = run().await {
        eprintln!("{message}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let path = socket_path()?;
    let parent = path
        .parent()
        .ok_or_else(|| "daemon socket has no parent directory".to_owned())?;
    create_private_runtime_dirs(parent)?;
    let lock = SingleInstance::acquire(&parent.join("daemon.lock"))?;
    prepare_socket_path(&path)?;
    let listener = UnixListener::bind(&path)
        .map_err(|error| format!("cannot bind daemon socket {}: {error}", path.display()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("cannot secure daemon socket: {error}"))?;
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| format!("cannot inspect daemon socket: {error}"))?;
    let _socket = SocketCleanup {
        path: path.clone(),
        device: metadata.dev(),
        inode: metadata.ino(),
    };
    let _lock = lock;

    let (actor_tx, actor_rx) = mpsc::channel(64);
    tokio::spawn(actor_loop(actor_rx, actor_tx.clone()));
    let connections = std::sync::Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|error| format!("cannot listen for SIGTERM: {error}"))?;

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(connection) => connection,
                    Err(error) => {
                        eprintln!("transient CLI accept failure: {error}");
                        continue;
                    }
                };
                let permit = match connections.clone().try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => continue,
                };
                let actor = actor_tx.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    let _ = serve(stream, actor).await;
                });
            }
            signal = tokio::signal::ctrl_c() => {
                signal.map_err(|error| format!("cannot listen for shutdown: {error}"))?;
                return Ok(());
            }
            _ = terminate.recv() => return Ok(()),
        }
    }
}

fn create_private_runtime_dirs(parent: &Path) -> Result<(), String> {
    let runtime = voisu_core::runtime_dir()?;
    let mut current = runtime;
    for component in ["voisu".to_owned(), format!("v{PROTOCOL_VERSION}")] {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(format!("unsafe runtime path component: {}", current.display()));
                }
                if metadata.uid() != unsafe { libc::geteuid() }
                    || metadata.mode() & 0o777 != 0o700
                {
                    return Err(format!("runtime directory is not private: {}", current.display()));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::DirBuilder::new()
                    .mode(0o700)
                    .create(&current)
                    .map_err(|error| format!("cannot create private runtime directory: {error}"))?;
            }
            Err(error) => return Err(format!("cannot inspect runtime directory: {error}")),
        }
    }
    if current != parent {
        return Err("unexpected daemon runtime directory".to_owned());
    }
    Ok(())
}

struct SingleInstance(File);

impl SingleInstance {
    fn acquire(path: &Path) -> Result<Self, String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .open(path)
            .map_err(|error| format!("cannot open daemon lock: {error}"))?;
        // SAFETY: flock only reads the valid file descriptor and flags.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result != 0 {
            return Err("voisu-daemon is already running".to_owned());
        }
        Ok(Self(file))
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        // SAFETY: this instance owns a valid open descriptor until Drop completes.
        let _ = unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn prepare_socket_path(path: &Path) -> Result<(), String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("cannot inspect daemon socket: {error}")),
    };
    if !metadata.file_type().is_socket() {
        return Err("refusing to replace unsafe daemon socket path".to_owned());
    }
    if std::os::unix::net::UnixStream::connect(path).is_ok() {
        return Err("voisu-daemon is already running".to_owned());
    }
    fs::remove_file(path).map_err(|error| format!("cannot remove stale daemon socket: {error}"))
}

struct SocketCleanup {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl Drop for SocketCleanup {
    fn drop(&mut self) {
        if let Ok(metadata) = fs::symlink_metadata(&self.path)
            && metadata.dev() == self.device
            && metadata.ino() == self.inode
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

enum ActorMessage {
    Command(Command, oneshot::Sender<Response>),
    Completed(Completion),
}

struct ActiveRecording {
    id: u64,
    capture: Box<dyn ActiveCapture>,
    providers: ProviderCoordinator,
    evidence: LifecycleEvidence,
}

enum ActorState {
    Idle,
    Recording(ActiveRecording),
    Processing(LifecycleEvidence),
}

struct Completion {
    id: u64,
    result: Result<(), BoundaryError>,
    evidence: LifecycleEvidence,
    validator: Box<dyn TranscriptValidator>,
    delivery: Box<dyn DeliveryAdapter>,
    reply: oneshot::Sender<Response>,
}

async fn actor_loop(mut rx: mpsc::Receiver<ActorMessage>, tx: mpsc::Sender<ActorMessage>) {
    let mut state = ActorState::Idle;
    let mut next_id = 1_u64;
    let mut capture: Box<dyn AudioCapture> = Box::new(ControlledCapture::from_env());
    let mut deepgram: Box<dyn TranscriptProvider> =
        Box::new(ControlledProvider::from_env(Provider::Deepgram));
    let mut groq: Box<dyn TranscriptProvider> =
        Box::new(ControlledProvider::from_env(Provider::Groq));
    let mut validator: Option<Box<dyn TranscriptValidator>> = Some(Box::new(ControlledValidator));
    let mut delivery: Option<Box<dyn DeliveryAdapter>> = Some(Box::new(ControlledDelivery));

    while let Some(message) = rx.recv().await {
        match message {
            ActorMessage::Command(command, reply) => match command {
                Command::Status => {
                    let response = status_response(&state);
                    let _ = reply.send(response);
                }
                Command::Start | Command::Toggle if matches!(state, ActorState::Idle) => {
                    let id = next_id;
                    next_id += 1;
                    let started = capture.begin(id).and_then(|active_capture| {
                        let deepgram = deepgram.start(id)?;
                        let groq = groq.start(id)?;
                        Ok((active_capture, ProviderStreams { deepgram, groq }))
                    });
                    match started {
                        Ok((active_capture, streams)) => {
                            let evidence = LifecycleEvidence {
                                recording_id: id,
                                stages: vec![
                                    LifecycleStage::CaptureStarted,
                                    LifecycleStage::ProvidersStarted,
                                ],
                                delivery_count: 0,
                            };
                            state = ActorState::Recording(ActiveRecording {
                                id,
                                capture: active_capture,
                                providers: ProviderCoordinator::start(PROVIDER_DEADLINE, streams),
                                evidence,
                            });
                            let _ = reply.send(Response::success(
                                DaemonState::Recording,
                                "Recording started",
                            ));
                        }
                        Err(error) => {
                            eprintln!("Recording {id}: {}", error.diagnostic());
                            let _ = reply.send(Response::rejected(
                                Some(DaemonState::Idle),
                                error.public_message(),
                            ));
                        }
                    }
                }
                Command::Start => {
                    let _ = reply.send(Response::rejected(
                        Some(state_label(&state)),
                        "Recording already active",
                    ));
                }
                Command::Stop | Command::Toggle if matches!(state, ActorState::Recording(_)) => {
                    let ActorState::Recording(recording) =
                        std::mem::replace(&mut state, ActorState::Idle)
                    else {
                        unreachable!()
                    };
                    state = ActorState::Processing(recording.evidence.clone());
                    let actor = tx.clone();
                    let current_validator = validator.take().expect("validator is available");
                    let current_delivery = delivery.take().expect("Delivery adapter is available");
                    tokio::spawn(process_recording(
                        recording,
                        current_validator,
                        current_delivery,
                        actor,
                        reply,
                    ));
                }
                Command::Stop => {
                    let _ = reply.send(Response::rejected(
                        Some(state_label(&state)),
                        if matches!(state, ActorState::Processing(_)) {
                            "Recording is being processed"
                        } else {
                            "No Recording active"
                        },
                    ));
                }
                Command::Toggle => {
                    let _ = reply.send(Response::rejected(
                        Some(DaemonState::Processing),
                        "Recording is being processed",
                    ));
                }
            },
            ActorMessage::Completed(completed) => {
                validator = Some(completed.validator);
                delivery = Some(completed.delivery);
                if matches!(&state, ActorState::Processing(evidence) if evidence.recording_id == completed.id)
                {
                    state = ActorState::Idle;
                    let response = match completed.result {
                        Ok(()) => Response::with_evidence(
                            true,
                            Some(DaemonState::Idle),
                            "Recording completed; Transcript delivered",
                            Some(completed.evidence),
                        ),
                        Err(error) => {
                            eprintln!("Recording {}: {}", completed.id, error.diagnostic());
                            Response::with_evidence(
                                false,
                                Some(DaemonState::Idle),
                                error.public_message(),
                                Some(completed.evidence),
                            )
                        }
                    };
                    let _ = completed.reply.send(response);
                }
            }
        }
    }
}

fn state_label(state: &ActorState) -> DaemonState {
    match state {
        ActorState::Idle => DaemonState::Idle,
        ActorState::Recording(_) => DaemonState::Recording,
        ActorState::Processing(_) => DaemonState::Processing,
    }
}

fn status_response(state: &ActorState) -> Response {
    let daemon_state = state_label(state);
    Response::with_evidence(
        true,
        Some(daemon_state),
        daemon_state.cli_label(),
        match state {
            ActorState::Recording(recording) => Some(recording.evidence.clone()),
            ActorState::Processing(evidence) => Some(evidence.clone()),
            ActorState::Idle => None,
        },
    )
}

async fn process_recording(
    mut recording: ActiveRecording,
    mut validator: Box<dyn TranscriptValidator>,
    mut delivery: Box<dyn DeliveryAdapter>,
    actor: mpsc::Sender<ActorMessage>,
    reply: oneshot::Sender<Response>,
) {
    let result = async {
        let audio = match recording.capture.finish().await {
            Ok(audio) => audio,
            Err(error) => {
                let _ = recording.capture.abort().await;
                recording.evidence.stages.push(LifecycleStage::CaptureAborted);
                return Err(error);
            }
        };
        recording.evidence.stages.push(LifecycleStage::CaptureFinalized);
        let sources = recording.providers.complete(audio).await?;
        recording.evidence.stages.push(LifecycleStage::ProvidersCompleted);
        let transcript = validator.validate(sources)?;
        recording.evidence.stages.push(LifecycleStage::ValidationCompleted);
        delivery.deliver(transcript).await?;
        recording.evidence.delivery_count += 1;
        recording.evidence.stages.push(LifecycleStage::DeliveryCompleted);
        Ok(())
    }
    .await;
    let _ = actor
        .send(ActorMessage::Completed(Completion {
            id: recording.id,
            result,
            evidence: recording.evidence,
            validator,
            delivery,
            reply,
        }))
        .await;
}

async fn serve(stream: UnixStream, actor: mpsc::Sender<ActorMessage>) -> Result<(), String> {
    let (reader, mut writer) = stream.into_split();
    let mut request = String::new();
    let mut limited = BufReader::new(reader).take(MAX_FRAME_BYTES + 1);
    timeout(IO_DEADLINE, limited.read_line(&mut request))
        .await
        .map_err(|_| "CLI read deadline elapsed".to_owned())?
        .map_err(|error| format!("cannot read CLI command: {error}"))?;
    if request.len() as u64 > MAX_FRAME_BYTES || !request.ends_with('\n') {
        return Err("CLI command frame is too large or incomplete".to_owned());
    }
    let envelope: VersionEnvelope = serde_json::from_str(&request)
        .map_err(|error| format!("cannot decode protocol envelope: {error}"))?;
    let response = if envelope.version != PROTOCOL_VERSION {
        Response::rejected(
            None,
            format!(
                "unsupported protocol version {}; expected {PROTOCOL_VERSION}",
                envelope.version
            ),
        )
    } else {
        let request: Request = serde_json::from_str(&request)
            .map_err(|error| format!("cannot decode CLI command: {error}"))?;
        let (reply_tx, reply_rx) = oneshot::channel();
        actor
            .send(ActorMessage::Command(request.command, reply_tx))
            .await
            .map_err(|_| "lifecycle actor is unavailable".to_owned())?;
        reply_rx
            .await
            .map_err(|_| "lifecycle actor dropped its response".to_owned())?
    };
    let mut encoded = serde_json::to_vec(&response)
        .map_err(|error| format!("cannot encode daemon response: {error}"))?;
    encoded.push(b'\n');
    timeout(IO_DEADLINE, writer.write_all(&encoded))
        .await
        .map_err(|_| "CLI write deadline elapsed".to_owned())?
        .map_err(|error| format!("cannot write daemon response: {error}"))
}

struct ControlledCapture {
    fail_finish_once: bool,
}

impl ControlledCapture {
    fn from_env() -> Self {
        Self {
            fail_finish_once: std::env::var_os("VOISU_TEST_CAPTURE_FINISH_FAILURE").is_some(),
        }
    }
}

impl AudioCapture for ControlledCapture {
    fn begin(&mut self, _recording_id: u64) -> Result<Box<dyn ActiveCapture>, BoundaryError> {
        let fail_finish = std::mem::take(&mut self.fail_finish_once);
        Ok(Box::new(ControlledActiveCapture { fail_finish }))
    }
}

struct ControlledActiveCapture {
    fail_finish: bool,
}

impl ActiveCapture for ControlledActiveCapture {
    fn finish(&mut self) -> BoundaryFuture<'_, CapturedAudio> {
        Box::pin(async move {
            if self.fail_finish {
                Err(BoundaryError::new(
                    BoundaryKind::Capture,
                    "controlled-secret-capture-detail",
                ))
            } else {
                Ok(CapturedAudio)
            }
        })
    }

    fn abort(self: Box<Self>) -> BoundaryFuture<'static, ()> {
        Box::pin(async { Ok(()) })
    }
}

struct ControlledProvider {
    provider: Provider,
    delay: Duration,
}

impl ControlledProvider {
    fn from_env(provider: Provider) -> Self {
        let delay = std::env::var("VOISU_TEST_PROVIDER_DELAY_MS")
            .ok()
            .and_then(|value| value.parse().ok())
            .map(Duration::from_millis)
            .unwrap_or_default();
        Self { provider, delay }
    }
}

impl TranscriptProvider for ControlledProvider {
    fn start(&mut self, _recording_id: u64) -> Result<Box<dyn ProviderStream>, BoundaryError> {
        Ok(Box::new(ControlledProviderStream {
            provider: self.provider,
            delay: self.delay,
        }))
    }
}

struct ControlledProviderStream {
    provider: Provider,
    delay: Duration,
}

impl ProviderStream for ControlledProviderStream {
    fn provider(&self) -> Provider {
        self.provider
    }

    fn send_audio(&mut self, _chunk: AudioChunk) -> BoundaryFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn complete(
        self: Box<Self>,
        _audio: CapturedAudio,
    ) -> BoundaryFuture<'static, SourceTranscript> {
        Box::pin(async move {
            tokio::time::sleep(self.delay).await;
            Ok(SourceTranscript {
                provider: self.provider,
                text: "controlled Source Transcript".to_owned(),
            })
        })
    }
}

struct ControlledValidator;

impl TranscriptValidator for ControlledValidator {
    fn validate(
        &mut self,
        sources: Vec<SourceTranscript>,
    ) -> Result<Transcript, BoundaryError> {
        sources
            .into_iter()
            .next()
            .map(|source| Transcript(source.text))
            .ok_or_else(|| BoundaryError::new(BoundaryKind::Validation, "no Source Transcript"))
    }
}

struct ControlledDelivery;

impl DeliveryAdapter for ControlledDelivery {
    fn deliver(&mut self, _transcript: Transcript) -> BoundaryFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}
