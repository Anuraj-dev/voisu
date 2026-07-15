use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot, Semaphore};
use tokio::task::JoinHandle;
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
    // Declared before the socket guard so it drops LAST on shutdown: the single-
    // instance lock must outlive socket cleanup, otherwise a replacement daemon
    // could acquire the lock and be spuriously rejected by the still-present socket.
    let _lock = SingleInstance::acquire(&parent.join("daemon.lock"))?;
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
    stop_tx: oneshot::Sender<()>,
    pump: JoinHandle<PumpOutput>,
    chunk_counter: Arc<AtomicU32>,
    evidence: LifecycleEvidence,
}

/// Ownership handed back by the capture pump once a Recording stops: the still-
/// live capture and provider coordinator, plus any error hit while streaming
/// live chunks to the providers.
struct PumpOutput {
    capture: Box<dyn ActiveCapture>,
    providers: ProviderCoordinator,
    stream_error: Option<BoundaryError>,
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
                    match begin_recording(&mut capture, &mut deepgram, &mut groq, id).await {
                        Ok((active_capture, streams)) => {
                            let evidence = LifecycleEvidence {
                                recording_id: id,
                                stages: vec![
                                    LifecycleStage::CaptureStarted,
                                    LifecycleStage::ProvidersStarted,
                                ],
                                delivery_count: 0,
                                streamed_chunk_count: 0,
                            };
                            let (stop_tx, stop_rx) = oneshot::channel();
                            let chunk_counter = Arc::new(AtomicU32::new(0));
                            let coordinator =
                                ProviderCoordinator::start(PROVIDER_DEADLINE, streams);
                            let pump = tokio::spawn(capture_pump(
                                active_capture,
                                coordinator,
                                stop_rx,
                                Arc::clone(&chunk_counter),
                            ));
                            state = ActorState::Recording(ActiveRecording {
                                id,
                                stop_tx,
                                pump,
                                chunk_counter,
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
                    let mut processing_evidence = recording.evidence.clone();
                    processing_evidence.streamed_chunk_count =
                        recording.chunk_counter.load(Ordering::SeqCst);
                    state = ActorState::Processing(processing_evidence);
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
            ActorState::Recording(recording) => {
                let mut evidence = recording.evidence.clone();
                evidence.streamed_chunk_count = recording.chunk_counter.load(Ordering::SeqCst);
                Some(evidence)
            }
            ActorState::Processing(evidence) => Some(evidence.clone()),
            ActorState::Idle => None,
        },
    )
}

/// Starts a Recording, aborting anything already started if a later step of the
/// start sequence fails so no capture or provider is left dangling. Capture-abort
/// errors are surfaced into the returned typed diagnostic rather than discarded.
async fn begin_recording(
    capture: &mut Box<dyn AudioCapture>,
    deepgram: &mut Box<dyn TranscriptProvider>,
    groq: &mut Box<dyn TranscriptProvider>,
    id: u64,
) -> Result<(Box<dyn ActiveCapture>, ProviderStreams), BoundaryError> {
    let active_capture = capture.begin(id)?;
    let deepgram_stream = match deepgram.start(id) {
        Ok(stream) => stream,
        Err(error) => return Err(abort_capture(active_capture, error).await),
    };
    let groq_stream = match groq.start(id) {
        Ok(stream) => stream,
        Err(error) => {
            // Deepgram already started: drop it (no provider abort seam) and
            // abort the capture so nothing is left running.
            drop(deepgram_stream);
            return Err(abort_capture(active_capture, error).await);
        }
    };
    Ok((
        active_capture,
        ProviderStreams {
            deepgram: deepgram_stream,
            groq: groq_stream,
        },
    ))
}

/// Aborts an already-started capture after another start step failed, folding any
/// abort failure into the originating diagnostic so it is never silently dropped.
async fn abort_capture(
    capture: Box<dyn ActiveCapture>,
    cause: BoundaryError,
) -> BoundaryError {
    match capture.abort().await {
        Ok(()) => cause,
        Err(abort_error) => combine_capture_abort(cause, abort_error),
    }
}

fn combine_capture_abort(cause: BoundaryError, abort_error: BoundaryError) -> BoundaryError {
    BoundaryError::new(
        cause.kind(),
        format!(
            "{}; capture abort failed: {}",
            cause.diagnostic(),
            abort_error.diagnostic()
        ),
    )
}

/// Owns the live capture and provider coordinator during a Recording, feeding
/// every captured chunk to BOTH providers as it arrives, until the Recording is
/// stopped. Hands ownership back so the stop path can finalize and complete.
async fn capture_pump(
    mut capture: Box<dyn ActiveCapture>,
    mut providers: ProviderCoordinator,
    mut stop_rx: oneshot::Receiver<()>,
    counter: Arc<AtomicU32>,
) -> PumpOutput {
    let mut stream_error = None;
    let mut draining = false;
    loop {
        if draining {
            // No further chunks to stream; hold the Recording open until stop.
            let _ = (&mut stop_rx).await;
            break;
        }
        tokio::select! {
            biased;
            _ = &mut stop_rx => break,
            result = capture.next_chunk() => match result {
                Ok(Some(chunk)) => {
                    if let Err(error) = providers.stream_audio(chunk).await {
                        stream_error = Some(error);
                        break;
                    }
                    counter.fetch_add(1, Ordering::SeqCst);
                }
                Ok(None) => draining = true,
                Err(error) => {
                    stream_error = Some(error);
                    break;
                }
            },
        }
    }
    PumpOutput {
        capture,
        providers,
        stream_error,
    }
}

async fn process_recording(
    recording: ActiveRecording,
    mut validator: Box<dyn TranscriptValidator>,
    mut delivery: Box<dyn DeliveryAdapter>,
    actor: mpsc::Sender<ActorMessage>,
    reply: oneshot::Sender<Response>,
) {
    let ActiveRecording {
        id,
        stop_tx,
        pump,
        chunk_counter,
        mut evidence,
    } = recording;
    let _ = stop_tx.send(());
    let PumpOutput {
        mut capture,
        providers,
        stream_error,
    } = pump.await.expect("capture pump should not panic");
    evidence.streamed_chunk_count = chunk_counter.load(Ordering::SeqCst);

    let result = async {
        if let Some(error) = stream_error {
            let abort = capture.abort().await;
            evidence.stages.push(LifecycleStage::CaptureAborted);
            return Err(match abort {
                Ok(()) => error,
                Err(abort_error) => combine_capture_abort(error, abort_error),
            });
        }
        let audio = match capture.finish().await {
            Ok(audio) => audio,
            Err(error) => {
                let abort = capture.abort().await;
                evidence.stages.push(LifecycleStage::CaptureAborted);
                return Err(match abort {
                    Ok(()) => error,
                    Err(abort_error) => combine_capture_abort(error, abort_error),
                });
            }
        };
        evidence.stages.push(LifecycleStage::CaptureFinalized);
        let sources = providers.complete(audio).await?;
        evidence.stages.push(LifecycleStage::ProvidersCompleted);
        let transcript = validator.validate(sources)?;
        evidence.stages.push(LifecycleStage::ValidationCompleted);
        delivery.deliver(transcript).await?;
        evidence.delivery_count += 1;
        evidence.stages.push(LifecycleStage::DeliveryCompleted);
        Ok(())
    }
    .await;
    let _ = actor
        .send(ActorMessage::Completed(Completion {
            id,
            result,
            evidence,
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
    fail_abort: bool,
    chunks: u32,
    chunk_delay: Duration,
}

impl ControlledCapture {
    fn from_env() -> Self {
        let chunks = std::env::var("VOISU_TEST_CAPTURE_CHUNKS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(1);
        let chunk_delay = std::env::var("VOISU_TEST_CHUNK_DELAY_MS")
            .ok()
            .and_then(|value| value.parse().ok())
            .map(Duration::from_millis)
            .unwrap_or_default();
        Self {
            fail_finish_once: std::env::var_os("VOISU_TEST_CAPTURE_FINISH_FAILURE").is_some(),
            fail_abort: std::env::var_os("VOISU_TEST_CAPTURE_ABORT_FAILURE").is_some(),
            chunks,
            chunk_delay,
        }
    }
}

impl AudioCapture for ControlledCapture {
    fn begin(&mut self, _recording_id: u64) -> Result<Box<dyn ActiveCapture>, BoundaryError> {
        let fail_finish = std::mem::take(&mut self.fail_finish_once);
        Ok(Box::new(ControlledActiveCapture {
            fail_finish,
            fail_abort: self.fail_abort,
            remaining_chunks: self.chunks,
            chunk_delay: self.chunk_delay,
        }))
    }
}

struct ControlledActiveCapture {
    fail_finish: bool,
    fail_abort: bool,
    remaining_chunks: u32,
    chunk_delay: Duration,
}

impl ActiveCapture for ControlledActiveCapture {
    fn next_chunk(&mut self) -> BoundaryFuture<'_, Option<AudioChunk>> {
        Box::pin(async move {
            if self.remaining_chunks == 0 {
                return Ok(None);
            }
            self.remaining_chunks -= 1;
            if !self.chunk_delay.is_zero() {
                tokio::time::sleep(self.chunk_delay).await;
            }
            Ok(Some(AudioChunk(vec![0])))
        })
    }

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
        let fail_abort = self.fail_abort;
        Box::pin(async move {
            if fail_abort {
                Err(BoundaryError::new(
                    BoundaryKind::Capture,
                    "controlled-abort-detail",
                ))
            } else {
                Ok(())
            }
        })
    }
}

struct ControlledProvider {
    provider: Provider,
    delay: Duration,
    fail_start_once: bool,
}

impl ControlledProvider {
    fn from_env(provider: Provider) -> Self {
        let delay = std::env::var("VOISU_TEST_PROVIDER_DELAY_MS")
            .ok()
            .and_then(|value| value.parse().ok())
            .map(Duration::from_millis)
            .unwrap_or_default();
        // Only Groq fails its start, so capture and Deepgram are already started
        // when the partial-start-failure abort path is exercised.
        let fail_start_once = provider == Provider::Groq
            && std::env::var_os("VOISU_TEST_PROVIDER_START_FAILURE").is_some();
        Self {
            provider,
            delay,
            fail_start_once,
        }
    }
}

impl TranscriptProvider for ControlledProvider {
    fn start(&mut self, _recording_id: u64) -> Result<Box<dyn ProviderStream>, BoundaryError> {
        if std::mem::take(&mut self.fail_start_once) {
            return Err(BoundaryError::new(
                BoundaryKind::Provider,
                "controlled-provider-start-detail",
            ));
        }
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
