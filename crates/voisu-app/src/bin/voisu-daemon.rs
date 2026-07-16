use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot, Semaphore};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use voisu_app::system::{
    CAPTURE_FINALIZE_DEADLINE, DeepgramProvider, FedoraShortcutPortal,
    GroqProvider, MergeResultValidator, PROVIDER_COMPLETION_DEADLINE, PipeWireCapture,
    PortalClipboardDelivery, RECONCILIATION_DEADLINE, RECOVERY_ABORT_DEADLINE,
};
use voisu_core::{
    ActiveCapture, AudioCapture, AudioChunk, BoundaryError, BoundaryFuture, BoundaryKind,
    CancelRegistry, CapturedAudio, Command, DaemonState, DeliveryAdapter, DeliveryMethod,
    DeliveryOutcome, DiagnosticRecord, DiagnosticStore, LifecycleEvidence, LifecycleStage,
    MergeResult, PROTOCOL_VERSION, Provider,
    ProviderCoordinator, ProviderStream, ProviderStreams, ReconciliationKind, ReconciliationModel,
    ReplayOutcome, Request, Response, RetentionPolicy, ShortcutPortal, SourceTranscript,
    SourceTranscriptRecord, Transcript, TranscriptDecision, TranscriptDecisionPipeline,
    TranscriptProvider, TranscriptValidator, TriggerKeyBinding, VersionEnvelope, replay_capture,
    socket_path,
};

const MAX_FRAME_BYTES: u64 = 16 * 1024;
const IO_DEADLINE: Duration = CAPTURE_FINALIZE_DEADLINE;
const MAX_CONNECTIONS: usize = 32;
const PROVIDER_DEADLINE: Duration = PROVIDER_COMPLETION_DEADLINE;

#[tokio::main]
async fn main() {
    let systemd_owned = matches!(
        std::env::args().skip(1).collect::<Vec<_>>().as_slice(),
        [argument] if argument == "--systemd"
    );
    if let Err(message) = run().await {
        // The service manager checks IPC before starting, but a manual daemon
        // may win the race after that check. A systemd-launched duplicate exits
        // cleanly so Restart=on-failure cannot create a crash loop.
        if systemd_owned && message == "voisu-daemon is already running" {
            eprintln!("manual or existing daemon detected; systemd instance not started");
            return;
        }
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

    // Correlated local diagnostics live under the already-hardened private
    // runtime directory and never leave the local machine. Startup cleanup
    // removes any debug audio or over-retention records a previous run left.
    let retention = RetentionPolicy::from_env();
    let diagnostics = Arc::new(
        DiagnosticStore::open(parent.join("diagnostics"), retention)
            .map_err(|error| format!("cannot open diagnostics store: {error}"))?,
    );
    if let Err(error) = diagnostics.cleanup_expired() {
        eprintln!("diagnostics cleanup failed: {error}");
    }

    let (actor_tx, actor_rx) = mpsc::channel(64);
    tokio::spawn(actor_loop(actor_rx, actor_tx.clone(), diagnostics));
    // The Global Shortcuts portal listener runs off the actor so a slow or
    // unavailable portal never blocks the IPC surface. Each Trigger Key
    // activation is fed to the actor as a Toggle, reusing the actor's
    // serialization so repeated or concurrent activations cannot overlap.
    // Disabling it (VOISU_DISABLE_SHORTCUTS) keeps the daemon usable in
    // sessions or tests that have no desktop portal.
    if std::env::var_os("VOISU_DISABLE_SHORTCUTS").is_none() {
        tokio::spawn(shortcut_listener(actor_tx.clone()));
    }
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
    Started(StartupCompletion),
    PumpTerminated(u64),
    Completed(Completion),
    Recovered(u64),
    ReplayCompleted(ReplayCompletion),
    /// The Global Shortcuts listener reports the desktop-approved Trigger Key
    /// binding once (or `None` when the portal is unavailable or denied), so the
    /// `Shortcut` command can display it. Binding never gates Recording control.
    ShortcutBound(Option<TriggerKeyBinding>),
}

/// Ownership handed back once a fixture replay finishes: the provider and
/// validation adapters return to the actor's pool so the daemon is reusable, and
/// the client reply carries the replay outcome.
struct ReplayCompletion {
    id: u64,
    deepgram: Box<dyn TranscriptProvider>,
    groq: Box<dyn TranscriptProvider>,
    validator: Box<dyn TranscriptValidator>,
    reply: oneshot::Sender<Response>,
    response: Response,
}

struct StartupCompletion {
    id: u64,
    capture: Box<dyn AudioCapture>,
    deepgram: Box<dyn TranscriptProvider>,
    groq: Box<dyn TranscriptProvider>,
    result: Result<(Box<dyn ActiveCapture>, ProviderStreams), StartFailure>,
    reply: oneshot::Sender<Response>,
}

/// A failed start sequence hands back everything it already started so the
/// actor can track the abort to completion before the daemon is reusable.
struct StartFailure {
    error: BoundaryError,
    capture: Option<Box<dyn ActiveCapture>>,
    provider_stream: Option<Box<dyn ProviderStream>>,
}

struct ActiveRecording {
    id: u64,
    stop_tx: oneshot::Sender<()>,
    pump: JoinHandle<PumpOutput>,
    chunk_counter: Arc<AtomicU32>,
    first_chunk_ms: Arc<AtomicU64>,
    started_at: Instant,
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
    Starting { id: u64, correlation_id: String },
    Recording(ActiveRecording),
    Processing(LifecycleEvidence),
    /// A failed start's aborts are still in flight. Publicly the daemon reads
    /// idle and stays responsive, but Start/Toggle are rejected with a
    /// retryable message until the cleanup acknowledges completion: deferring
    /// commands instead would reorder them and could begin a Recording whose
    /// client already gave up (see docs/decisions.md).
    Recovering(u64),
    /// A fixture replay is borrowing the provider and validation adapters. The
    /// daemon reads idle but rejects Start/Toggle until the replay returns them.
    Replaying(u64),
}

struct Completion {
    id: u64,
    result: Result<(), BoundaryError>,
    evidence: LifecycleEvidence,
    validator: Box<dyn TranscriptValidator>,
    delivery: Box<dyn DeliveryAdapter>,
    reply: Option<oneshot::Sender<Response>>,
}

async fn actor_loop(
    mut rx: mpsc::Receiver<ActorMessage>,
    tx: mpsc::Sender<ActorMessage>,
    diagnostics: Arc<DiagnosticStore>,
) {
    let mut state = ActorState::Idle;
    let mut next_id = 1_u64;
    // Raw audio is retained only when the user explicitly enables debug capture.
    let debug_capture = std::env::var_os("VOISU_DEBUG_CAPTURE").is_some();
    let test_mode = std::env::var_os("VOISU_TEST_MODE");
    let controlled = test_mode.as_deref() == Some(std::ffi::OsStr::new("controlled"));
    let controlled_deadlines = controlled
        || test_mode.as_deref() == Some(std::ffi::OsStr::new("system-boundaries"));
    let mut capture: Option<Box<dyn AudioCapture>> = Some(if controlled {
        Box::new(ControlledCapture::from_env())
    } else {
        Box::new(PipeWireCapture)
    });
    let mut deepgram: Option<Box<dyn TranscriptProvider>> = Some(if controlled {
        Box::new(ControlledProvider::from_env(Provider::Deepgram))
    } else {
        Box::new(DeepgramProvider)
    });
    let mut groq: Option<Box<dyn TranscriptProvider>> = Some(if controlled {
        Box::new(ControlledProvider::from_env(Provider::Groq))
    } else {
        Box::new(GroqProvider)
    });
    let mut validator: Option<Box<dyn TranscriptValidator>> = if controlled {
        Some(Box::new(ControlledValidator::from_env()))
    } else {
        Some(Box::new(MergeResultValidator::new()))
    };
    let mut delivery: Option<Box<dyn DeliveryAdapter>> = if controlled {
        Some(Box::new(ControlledDelivery))
    } else if std::env::var_os("VOISU_DISABLE_DIRECT_DELIVERY").is_some() {
        Some(Box::new(PortalClipboardDelivery::clipboard_only()))
    } else {
        Some(Box::new(PortalClipboardDelivery::default()))
    };
    // The desktop-approved Trigger Key binding, once the portal listener reports
    // it. `None` means no Trigger Key is bound (portal unavailable or denied),
    // which never prevents CLI Recording control.
    let mut shortcut_binding: Option<TriggerKeyBinding> = None;

    while let Some(message) = rx.recv().await {
        match message {
            ActorMessage::Command(command, reply) => match command {
                Command::Status => {
                    let response = status_response(&state);
                    let _ = reply.send(response);
                }
                Command::Shortcut => {
                    let message = match &shortcut_binding {
                        Some(binding) => format!("Trigger Key: {}", binding.description),
                        None => "No Trigger Key is bound; start, stop, and toggle \
                                 remain available"
                            .to_owned(),
                    };
                    let _ = reply.send(Response::success(state_label(&state), message));
                }
                Command::History => {
                    let records = diagnostics.history().unwrap_or_default();
                    let _ = reply.send(Response::with_history(records));
                }
                Command::Export(correlation_id) => {
                    let response = match diagnostics.find(&correlation_id) {
                        Ok(Some(record)) => Response::with_export(voisu_core::export_record(
                            record,
                            std::env::vars(),
                        )),
                        Ok(None) => Response::rejected(
                            Some(state_label(&state)),
                            "no diagnostic record for that correlation ID",
                        ),
                        Err(_) => Response::rejected(
                            Some(state_label(&state)),
                            "diagnostics are unavailable",
                        ),
                    };
                    let _ = reply.send(response);
                }
                Command::Replay(fixture_name) if matches!(state, ActorState::Idle) => {
                    let id = next_id;
                    next_id += 1;
                    state = ActorState::Replaying(id);
                    let current_deepgram =
                        deepgram.take().expect("Deepgram adapter is available");
                    let current_groq = groq.take().expect("Groq adapter is available");
                    let current_validator = validator.take().expect("validator is available");
                    let provider_deadline = if controlled_deadlines
                        && std::env::var_os("VOISU_TEST_PROVIDER_DEADLINE_MS").is_some()
                    {
                        env_millis("VOISU_TEST_PROVIDER_DEADLINE_MS").max(Duration::from_millis(1))
                    } else {
                        PROVIDER_DEADLINE
                    };
                    // The replay runs supervised: its JoinHandle is awaited by a
                    // wrapper that reports completion on EVERY path, including a
                    // panic — otherwise a panic would drop the borrowed adapters
                    // and wedge the daemon in Replaying forever.
                    let replay = tokio::spawn(replay_recording(
                        fixture_name,
                        id,
                        diagnostics.fixture_dir(),
                        current_deepgram,
                        current_groq,
                        current_validator,
                        provider_deadline,
                    ));
                    tokio::spawn(supervise_replay(replay, id, controlled, reply, tx.clone()));
                }
                Command::Replay(_) => {
                    let _ = reply.send(Response::rejected(
                        Some(state_label(&state)),
                        "cannot replay a fixture while a Recording is active",
                    ));
                }
                Command::Start | Command::Toggle
                    if matches!(state, ActorState::Recovering(_) | ActorState::Replaying(_)) =>
                {
                    // Immediate retryable rejection: replaying deferred
                    // commands would reorder Start/Stop and could begin a
                    // Recording after its client timed out.
                    let _ = reply.send(Response::rejected(
                        Some(DaemonState::Idle),
                        "Recording recovery in progress; retry shortly",
                    ));
                }
                Command::Start | Command::Toggle if matches!(state, ActorState::Idle) => {
                    let id = next_id;
                    next_id += 1;
                    // The correlation ID exists from the moment the Recording is
                    // accepted, so startup failures and recovery evidence are
                    // correlated even though no adapter has started yet.
                    state = ActorState::Starting {
                        id,
                        correlation_id: voisu_core::correlation_id(id),
                    };
                    let mut current_capture = capture.take().expect("capture adapter is available");
                    let mut current_deepgram =
                        deepgram.take().expect("Deepgram adapter is available");
                    let mut current_groq = groq.take().expect("Groq adapter is available");
                    let actor = tx.clone();
                    tokio::task::spawn_blocking(move || {
                        let result = begin_recording(
                            &mut current_capture,
                            &mut current_deepgram,
                            &mut current_groq,
                            id,
                        );
                        let _ = actor.blocking_send(ActorMessage::Started(StartupCompletion {
                            id,
                            capture: current_capture,
                            deepgram: current_deepgram,
                            groq: current_groq,
                            result,
                            reply,
                        }));
                    });
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
                    processing_evidence.first_chunk_ms =
                        atomic_millis(&recording.first_chunk_ms);
                    state = ActorState::Processing(processing_evidence);
                    let actor = tx.clone();
                    let current_validator = validator.take().expect("validator is available");
                    let current_delivery = delivery.take().expect("Delivery adapter is available");
                    tokio::spawn(process_recording(
                        recording,
                        current_validator,
                        current_delivery,
                        actor,
                        Some(reply),
                        Arc::clone(&diagnostics),
                        debug_capture,
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
                        Some(state_label(&state)),
                        if matches!(state, ActorState::Processing(_)) {
                            "Recording is being processed"
                        } else {
                            "Recording is starting"
                        },
                    ));
                }
            },
            ActorMessage::Started(started) => {
                let StartupCompletion {
                    id,
                    capture: returned_capture,
                    deepgram: returned_deepgram,
                    groq: returned_groq,
                    result,
                    reply,
                } = started;
                capture = Some(returned_capture);
                deepgram = Some(returned_deepgram);
                groq = Some(returned_groq);
                let correlation = match &state {
                    ActorState::Starting {
                        id: starting_id,
                        correlation_id,
                    } if *starting_id == id => Some(correlation_id.clone()),
                    _ => None,
                };
                if let Some(correlation) = correlation {
                    match result {
                        Ok((active_capture, streams)) => {
                            let evidence = base_evidence(
                                id,
                                correlation,
                                vec![
                                    LifecycleStage::CaptureStarted,
                                    LifecycleStage::ProvidersStarted,
                                ],
                            );
                            let (stop_tx, stop_rx) = oneshot::channel();
                            let chunk_counter = Arc::new(AtomicU32::new(0));
                            let first_chunk_ms = Arc::new(AtomicU64::new(u64::MAX));
                            let started_at = Instant::now();
                            let provider_deadline = if controlled_deadlines
                                && std::env::var_os("VOISU_TEST_PROVIDER_DEADLINE_MS").is_some()
                            {
                                env_millis("VOISU_TEST_PROVIDER_DEADLINE_MS")
                                    .max(Duration::from_millis(1))
                            } else {
                                PROVIDER_DEADLINE
                            };
                            let coordinator = ProviderCoordinator::start(
                                provider_deadline,
                                RECOVERY_ABORT_DEADLINE,
                                streams,
                            );
                            let pump = tokio::spawn(capture_pump(
                                id,
                                active_capture,
                                coordinator,
                                stop_rx,
                                Arc::clone(&chunk_counter),
                                Arc::clone(&first_chunk_ms),
                                started_at,
                                tx.clone(),
                            ));
                            state = ActorState::Recording(ActiveRecording {
                                id,
                                stop_tx,
                                pump,
                                chunk_counter,
                                first_chunk_ms,
                                started_at,
                                evidence,
                            });
                            let _ = reply.send(Response::success(
                                DaemonState::Recording,
                                "Recording started",
                            ));
                        }
                        Err(failure) => {
                            let recovering =
                                failure.capture.is_some() || failure.provider_stream.is_some();
                            eprintln!(
                                "Recording {id} [{correlation}]: {}",
                                failure.error.diagnostic()
                            );
                            // A startup failure is correlated and retained like
                            // any other Recording outcome: its record persists
                            // locally and the rejection carries the correlated
                            // evidence back to the client.
                            let mut record = DiagnosticRecord::new(correlation.clone(), id);
                            record.error =
                                Some(failure.error.public_message().to_owned());
                            record.recovery_attempted = recovering;
                            if let Err(error) = diagnostics.record(record) {
                                eprintln!(
                                    "Recording {id} [{correlation}]: writing diagnostics failed: {error}"
                                );
                            }
                            let mut evidence =
                                base_evidence(id, correlation.clone(), Vec::new());
                            evidence.recovery_attempted = recovering;
                            let _ = reply.send(Response::with_evidence(
                                false,
                                Some(DaemonState::Idle),
                                failure.error.public_message(),
                                Some(evidence),
                            ));
                            if recovering {
                                // The daemon reads idle immediately but rejects
                                // new Recordings until the bounded aborts
                                // acknowledge completion (ActorMessage::Recovered).
                                state = ActorState::Recovering(id);
                                tokio::spawn(recover_failed_start(
                                    id,
                                    correlation,
                                    failure,
                                    tx.clone(),
                                ));
                            } else {
                                state = ActorState::Idle;
                            }
                        }
                    }
                }
            }
            ActorMessage::Recovered(id) => {
                if matches!(&state, ActorState::Recovering(recovering) if *recovering == id) {
                    state = ActorState::Idle;
                }
            }
            ActorMessage::ShortcutBound(binding) => {
                shortcut_binding = binding;
            }
            ActorMessage::ReplayCompleted(completed) => {
                let ReplayCompletion {
                    id,
                    deepgram: returned_deepgram,
                    groq: returned_groq,
                    validator: returned_validator,
                    reply,
                    response,
                } = completed;
                deepgram = Some(returned_deepgram);
                groq = Some(returned_groq);
                validator = Some(returned_validator);
                if matches!(&state, ActorState::Replaying(replaying) if *replaying == id) {
                    state = ActorState::Idle;
                }
                let _ = reply.send(response);
            }
            ActorMessage::PumpTerminated(id) => {
                if matches!(&state, ActorState::Recording(recording) if recording.id == id) {
                    let ActorState::Recording(recording) =
                        std::mem::replace(&mut state, ActorState::Idle)
                    else {
                        unreachable!()
                    };
                    let mut processing_evidence = recording.evidence.clone();
                    processing_evidence.streamed_chunk_count =
                        recording.chunk_counter.load(Ordering::SeqCst);
                    processing_evidence.first_chunk_ms =
                        atomic_millis(&recording.first_chunk_ms);
                    state = ActorState::Processing(processing_evidence);
                    let current_validator = validator.take().expect("validator is available");
                    let current_delivery = delivery.take().expect("Delivery adapter is available");
                    tokio::spawn(process_recording(
                        recording,
                        current_validator,
                        current_delivery,
                        tx.clone(),
                        None,
                        Arc::clone(&diagnostics),
                        debug_capture,
                    ));
                }
            }
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
                            match completed.evidence.delivery_method {
                                Some(DeliveryMethod::ClipboardFallback) => {
                                    "Direct Delivery unavailable; Transcript is on the clipboard"
                                }
                                _ => "Transcript submitted through the compositor; preserved on the clipboard",
                            },
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
                    if let Some(reply) = completed.reply {
                        let _ = reply.send(response);
                    }
                }
            }
        }
    }
}

fn state_label(state: &ActorState) -> DaemonState {
    match state {
        ActorState::Idle | ActorState::Recovering(_) | ActorState::Replaying(_) => {
            DaemonState::Idle
        }
        ActorState::Starting { .. } => DaemonState::Recording,
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
                evidence.first_chunk_ms = atomic_millis(&recording.first_chunk_ms);
                Some(evidence)
            }
            ActorState::Processing(evidence) => Some(evidence.clone()),
            ActorState::Idle
            | ActorState::Starting { .. }
            | ActorState::Recovering(_)
            | ActorState::Replaying(_) => None,
        },
    )
}

/// A fresh lifecycle evidence skeleton stamped with the Recording's correlation
/// ID, shared by the successful-start, startup-failure, and replay paths.
fn base_evidence(
    recording_id: u64,
    correlation_id: String,
    stages: Vec<LifecycleStage>,
) -> LifecycleEvidence {
    LifecycleEvidence {
        recording_id,
        correlation_id,
        stages,
        delivery_count: 0,
        delivery_method: None,
        delivery_fallback_reason: None,
        streamed_chunk_count: 0,
        source_transcript_providers: Vec::new(),
        first_chunk_ms: None,
        capture_finalized_ms: None,
        provider_timings_ms: Vec::new(),
        release_to_text_ms: None,
        transcript_selection: None,
        validation_reason: None,
        fallback_reason: None,
        reconciliation_requested: false,
        recovery_attempted: false,
    }
}

fn atomic_millis(value: &AtomicU64) -> Option<u64> {
    let value = value.load(Ordering::SeqCst);
    (value != u64::MAX).then_some(value)
}

fn elapsed_millis(started_at: Instant) -> u64 {
    u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Starts a Recording. If a later step of the start sequence fails, everything
/// already started is handed back in the failure so the actor can run the
/// aborts off its loop and only become reusable once they acknowledge.
fn begin_recording(
    capture: &mut Box<dyn AudioCapture>,
    deepgram: &mut Box<dyn TranscriptProvider>,
    groq: &mut Box<dyn TranscriptProvider>,
    id: u64,
) -> Result<(Box<dyn ActiveCapture>, ProviderStreams), StartFailure> {
    let active_capture = match capture.begin(id) {
        Ok(active_capture) => active_capture,
        Err(error) => {
            return Err(StartFailure {
                error,
                capture: None,
                provider_stream: None,
            });
        }
    };
    let deepgram_stream = match deepgram.start(id) {
        Ok(stream) => stream,
        Err(error) => {
            return Err(StartFailure {
                error,
                capture: Some(active_capture),
                provider_stream: None,
            });
        }
    };
    let groq_stream = match groq.start(id) {
        Ok(stream) => stream,
        Err(error) => {
            return Err(StartFailure {
                error,
                capture: Some(active_capture),
                provider_stream: Some(deepgram_stream),
            });
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

/// Aborts everything a failed start already started, off the actor loop, then
/// acknowledges completion so the actor can leave Recovering. Abort failures
/// and timeouts are surfaced into local diagnostics rather than discarded.
async fn recover_failed_start(
    id: u64,
    correlation: String,
    failure: StartFailure,
    actor: mpsc::Sender<ActorMessage>,
) {
    let StartFailure {
        capture,
        provider_stream,
        ..
    } = failure;
    let correlation = correlation.as_str();
    let capture_abort = async {
        if let Some(capture) = capture {
            match timeout(RECOVERY_ABORT_DEADLINE, capture.abort()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    eprintln!(
                        "Recording {id} [{correlation}]: capture abort failed: {}",
                        error.diagnostic()
                    );
                }
                Err(_) => {
                    eprintln!("Recording {id} [{correlation}]: capture abort timed out")
                }
            }
        }
    };
    let provider_abort = async {
        if let Some(stream) = provider_stream {
            match timeout(RECOVERY_ABORT_DEADLINE, stream.abort()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    eprintln!(
                        "Recording {id} [{correlation}]: provider abort failed: {}",
                        error.diagnostic()
                    );
                }
                Err(_) => {
                    eprintln!("Recording {id} [{correlation}]: provider abort timed out")
                }
            }
        }
    };
    tokio::join!(capture_abort, provider_abort);
    let _ = actor.send(ActorMessage::Recovered(id)).await;
}

/// Awaits a capture abort with a bounded deadline, folding any abort failure or
/// timeout into the originating diagnostic so it is never silently dropped.
async fn bounded_abort(capture: Box<dyn ActiveCapture>, cause: BoundaryError) -> BoundaryError {
    match timeout(RECOVERY_ABORT_DEADLINE, capture.abort()).await {
        Ok(Ok(())) => cause,
        Ok(Err(abort_error)) => combine_capture_abort(cause, abort_error),
        Err(_) => BoundaryError::new(
            cause.kind(),
            format!("{}; capture abort timed out", cause.diagnostic()),
        ),
    }
}

/// Aborts BOTH the still-live capture and the provider coordinator when a
/// Recording fails, concurrently and bounded: dropping the coordinator would
/// detach its spawned provider work, leaving requests from the failed
/// Recording live while the next Recording is accepted.
async fn abort_recording_work(
    capture: Box<dyn ActiveCapture>,
    providers: ProviderCoordinator,
    cause: BoundaryError,
) -> BoundaryError {
    let (cause, provider_result) = tokio::join!(
        bounded_abort(capture, cause),
        timeout(RECOVERY_ABORT_DEADLINE, providers.abort())
    );
    match provider_result {
        Ok(Ok(())) => cause,
        Ok(Err(error)) => BoundaryError::new(
            cause.kind(),
            format!(
                "{}; provider abort failed: {}",
                cause.diagnostic(),
                error.diagnostic()
            ),
        ),
        Err(_) => BoundaryError::new(
            cause.kind(),
            format!("{}; provider abort timed out", cause.diagnostic()),
        ),
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
    id: u64,
    mut capture: Box<dyn ActiveCapture>,
    mut providers: ProviderCoordinator,
    mut stop_rx: oneshot::Receiver<()>,
    counter: Arc<AtomicU32>,
    first_chunk_ms: Arc<AtomicU64>,
    started_at: Instant,
    actor: mpsc::Sender<ActorMessage>,
) -> PumpOutput {
    let mut stream_error = None;
    loop {
        tokio::select! {
            biased;
            _ = &mut stop_rx => break,
            result = capture.next_chunk() => match result {
                Ok(Some(chunk)) => {
                    // The provider send must also race the stop signal: a
                    // stalled provider send must never prevent the pump from
                    // observing stop and releasing the Recording.
                    tokio::select! {
                        biased;
                        _ = &mut stop_rx => break,
                        sent = providers.stream_audio(chunk) => match sent {
                            Ok(()) => {
                                counter.fetch_add(1, Ordering::SeqCst);
                                let _ = first_chunk_ms.compare_exchange(
                                    u64::MAX,
                                    elapsed_millis(started_at),
                                    Ordering::SeqCst,
                                    Ordering::SeqCst,
                                );
                            }
                            Err(error) => {
                                stream_error = Some(error);
                                break;
                            }
                        },
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    stream_error = Some(error);
                    break;
                }
            },
        }
    }
    let _ = actor.send(ActorMessage::PumpTerminated(id)).await;
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
    reply: Option<oneshot::Sender<Response>>,
    diagnostics: Arc<DiagnosticStore>,
    debug_capture: bool,
) {
    let ActiveRecording {
        id,
        stop_tx,
        pump,
        chunk_counter,
        first_chunk_ms,
        started_at,
        mut evidence,
    } = recording;
    let _ = stop_tx.send(());
    let PumpOutput {
        mut capture,
        providers,
        stream_error,
    } = pump.await.expect("capture pump should not panic");
    evidence.streamed_chunk_count = chunk_counter.load(Ordering::SeqCst);
    evidence.first_chunk_ms = atomic_millis(&first_chunk_ms);

    let correlation_id = evidence.correlation_id.clone();
    // Local diagnostic evidence collected alongside the lifecycle evidence and
    // persisted to bounded local history once the Recording completes. Raw audio
    // is captured only when the user explicitly enabled debug capture.
    let mut source_records: Vec<SourceTranscriptRecord> = Vec::new();
    let mut final_transcript: Option<String> = None;
    let mut debug_audio = None;

    let result = async {
        if let Some(error) = stream_error {
            evidence.stages.push(LifecycleStage::CaptureAborted);
            return Err(abort_recording_work(capture, providers, error).await);
        }
        let audio = match capture.finish().await {
            Ok(audio) => audio,
            Err(error) => {
                evidence.stages.push(LifecycleStage::CaptureAborted);
                return Err(abort_recording_work(capture, providers, error).await);
            }
        };
        evidence.capture_finalized_ms = Some(elapsed_millis(started_at));
        evidence.stages.push(LifecycleStage::CaptureFinalized);
        if debug_capture {
            match diagnostics.store_debug_audio(&correlation_id, audio.pcm_s16le_mono_16khz()) {
                Ok(record) => debug_audio = Some(record),
                Err(error) => eprintln!("Recording {id}: debug audio capture failed: {error}"),
            }
        }
        let completed = providers.complete_with_timings(audio).await?;
        let sources = completed.sources;
        evidence.provider_timings_ms = completed.timings_ms;
        evidence.source_transcript_providers =
            sources.iter().map(|source| source.provider).collect();
        source_records = sources.iter().map(SourceTranscriptRecord::new).collect();
        evidence.stages.push(LifecycleStage::ProvidersCompleted);
        let decision = match validator.validate(sources).await {
            Ok(decision) => decision,
            Err(error) => {
                if let Some(failure) = error.transcript_failure() {
                    evidence.validation_reason = Some(failure.validation_reason.clone());
                    evidence.fallback_reason = failure.fallback_reason.clone();
                    evidence.reconciliation_requested = failure.reconciliation_requested;
                    evidence.recovery_attempted = failure.recovery_attempted;
                } else {
                    evidence.validation_reason = Some(error.diagnostic().to_owned());
                }
                return Err(error);
            }
        };
        evidence.transcript_selection = Some(decision.selection);
        evidence.validation_reason = Some(decision.validation_reason);
        evidence.fallback_reason = decision.fallback_reason;
        evidence.reconciliation_requested = decision.reconciliation_requested;
        evidence.recovery_attempted = decision.recovery_attempted;
        evidence.stages.push(LifecycleStage::ValidationCompleted);
        final_transcript = Some(decision.transcript.0.clone());
        let delivery_outcome = delivery.deliver(decision.transcript).await?;
        evidence.delivery_count += 1;
        evidence.delivery_method = Some(delivery_outcome.method);
        evidence.delivery_fallback_reason = delivery_outcome.fallback_reason;
        evidence.release_to_text_ms = Some(elapsed_millis(started_at));
        evidence.stages.push(LifecycleStage::DeliveryCompleted);
        Ok(())
    }
    .await;

    let record = diagnostic_record(
        &evidence,
        source_records,
        final_transcript,
        debug_audio,
        result.as_ref().err(),
    );
    if let Err(error) = diagnostics.record(record) {
        eprintln!("Recording {id}: writing diagnostics failed: {error}");
    }

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

/// Builds the persisted diagnostic record for one completed Recording from its
/// lifecycle evidence and the collected transcripts. The public error message
/// (never the internal diagnostic) is recorded so history never leaks a secret.
fn diagnostic_record(
    evidence: &LifecycleEvidence,
    source_transcripts: Vec<SourceTranscriptRecord>,
    final_transcript: Option<String>,
    debug_audio: Option<voisu_core::DebugAudioRecord>,
    error: Option<&BoundaryError>,
) -> DiagnosticRecord {
    let mut record = DiagnosticRecord::new(evidence.correlation_id.clone(), evidence.recording_id);
    record.stages = evidence.stages.clone();
    record.streamed_chunk_count = evidence.streamed_chunk_count;
    record.source_transcripts = source_transcripts;
    if let Some(text) = final_transcript {
        record.set_final_transcript(text);
    }
    record.selection = evidence.transcript_selection;
    record.validation_reason = evidence.validation_reason.clone();
    record.fallback_reason = evidence.fallback_reason.clone();
    record.reconciliation_requested = evidence.reconciliation_requested;
    record.recovery_attempted = evidence.recovery_attempted;
    record.delivery_count = evidence.delivery_count;
    record.delivery_method = evidence.delivery_method;
    record.delivery_fallback_reason = evidence.delivery_fallback_reason.clone();
    record.first_chunk_ms = evidence.first_chunk_ms;
    record.capture_finalized_ms = evidence.capture_finalized_ms;
    record.provider_timings_ms = evidence.provider_timings_ms.clone();
    record.release_to_text_ms = evidence.release_to_text_ms;
    record.error = error.map(|error| error.public_message().to_owned());
    record.debug_audio = debug_audio;
    record
}

/// The adapters and response a finished replay hands back through its
/// supervisor. Adapters travel inside the task's return value, so the ONLY way
/// they can be lost is a panic — which the supervisor detects and repairs.
struct ReplayResult {
    deepgram: Box<dyn TranscriptProvider>,
    groq: Box<dyn TranscriptProvider>,
    validator: Box<dyn TranscriptValidator>,
    response: Response,
}

/// Awaits the supervised replay task and reports completion to the actor on
/// EVERY path. If the replay panicked, the borrowed adapters were dropped with
/// it, so the supervisor rebuilds fresh ones (the adapters are stateless
/// constructors) and completes with an error — the daemon must never wedge in
/// Replaying.
async fn supervise_replay(
    replay: JoinHandle<ReplayResult>,
    id: u64,
    controlled: bool,
    reply: oneshot::Sender<Response>,
    actor: mpsc::Sender<ActorMessage>,
) {
    let completion = match replay.await {
        Ok(result) => ReplayCompletion {
            id,
            deepgram: result.deepgram,
            groq: result.groq,
            validator: result.validator,
            reply,
            response: result.response,
        },
        Err(join_error) => {
            eprintln!("Replay {id}: replay task failed: {join_error}");
            let (deepgram, groq, validator) = rebuild_replay_adapters(controlled);
            ReplayCompletion {
                id,
                deepgram,
                groq,
                validator,
                reply,
                response: Response::rejected(Some(DaemonState::Idle), "fixture replay failed"),
            }
        }
    };
    let _ = actor.send(ActorMessage::ReplayCompleted(completion)).await;
}

/// Rebuilds the provider and validation adapters after a replay panic dropped
/// the originals. Mirrors the actor's startup construction.
fn rebuild_replay_adapters(
    controlled: bool,
) -> (
    Box<dyn TranscriptProvider>,
    Box<dyn TranscriptProvider>,
    Box<dyn TranscriptValidator>,
) {
    if controlled {
        (
            Box::new(ControlledProvider::from_env(Provider::Deepgram)),
            Box::new(ControlledProvider::from_env(Provider::Groq)),
            Box::new(ControlledValidator::from_env()),
        )
    } else {
        (
            Box::new(DeepgramProvider),
            Box::new(GroqProvider),
            Box::new(MergeResultValidator::new()),
        )
    }
}

/// Replays a fixed captured fixture named `fixture_name` — which must live
/// inside the approved private fixture directory — through the provider and
/// validation boundaries without capturing audio, then returns the borrowed
/// adapters through its supervisor so the daemon is reusable. The fixture is
/// raw s16le/mono/16 kHz PCM, the same format capture produces.
async fn replay_recording(
    fixture_name: String,
    id: u64,
    fixture_dir: PathBuf,
    mut deepgram: Box<dyn TranscriptProvider>,
    mut groq: Box<dyn TranscriptProvider>,
    mut validator: Box<dyn TranscriptValidator>,
    provider_deadline: Duration,
) -> ReplayResult {
    let response = match run_replay(
        &fixture_name,
        &fixture_dir,
        id,
        &mut deepgram,
        &mut groq,
        validator.as_mut(),
        provider_deadline,
    )
    .await
    {
        Ok(outcome) => {
            let mut evidence = base_evidence(
                id,
                voisu_core::correlation_id(id),
                vec![
                    LifecycleStage::ProvidersCompleted,
                    LifecycleStage::ValidationCompleted,
                ],
            );
            evidence.source_transcript_providers = outcome
                .source_transcripts
                .iter()
                .map(|source| source.provider)
                .collect();
            evidence.provider_timings_ms = outcome.timings_ms;
            evidence.transcript_selection = Some(outcome.decision.selection);
            evidence.validation_reason = Some(outcome.decision.validation_reason.clone());
            evidence.fallback_reason = outcome.decision.fallback_reason.clone();
            evidence.reconciliation_requested = outcome.decision.reconciliation_requested;
            evidence.recovery_attempted = outcome.decision.recovery_attempted;
            Response::with_evidence(
                true,
                Some(DaemonState::Idle),
                format!(
                    "replayed fixture through {} Source Transcript(s)",
                    outcome.source_transcripts.len()
                ),
                Some(evidence),
            )
        }
        Err(error) => {
            eprintln!("Replay {id}: {}", error.diagnostic());
            Response::rejected(Some(DaemonState::Idle), error.public_message())
        }
    };
    ReplayResult {
        deepgram,
        groq,
        validator,
        response,
    }
}

const MAX_FIXTURE_BYTES: u64 = 32 * 1024 * 1024;

/// Opens and reads a replay fixture with no TOCTOU window: the name must be a
/// plain component inside the approved fixture directory, the open itself
/// refuses symlinks (O_NOFOLLOW) and never blocks on a FIFO (O_NONBLOCK), and
/// every property — regular file, owner, size — is validated on the OPENED
/// descriptor before a bounded read from that same descriptor. Nothing outside
/// the fixture directory can ever be sent to a provider.
fn read_fixture(fixture_dir: &Path, name: &str) -> Result<Vec<u8>, BoundaryError> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
    {
        return Err(BoundaryError::new(
            BoundaryKind::Capture,
            "fixture must be a plain file name inside the fixture directory",
        ));
    }
    let path = fixture_dir.join(name);
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(&path)
        .map_err(|error| {
            BoundaryError::new(BoundaryKind::Capture, format!("cannot open fixture: {error}"))
        })?;
    let metadata = file.metadata().map_err(|error| {
        BoundaryError::new(BoundaryKind::Capture, format!("cannot inspect fixture: {error}"))
    })?;
    if !metadata.is_file() {
        return Err(BoundaryError::new(
            BoundaryKind::Capture,
            "fixture must be a regular file",
        ));
    }
    // SAFETY: geteuid has no preconditions and does not mutate memory.
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(BoundaryError::new(
            BoundaryKind::Capture,
            "fixture must be owned by the current user",
        ));
    }
    if metadata.len() > MAX_FIXTURE_BYTES {
        return Err(BoundaryError::new(BoundaryKind::Capture, "fixture is too large"));
    }
    let mut bytes = Vec::new();
    std::io::Read::take(file, MAX_FIXTURE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            BoundaryError::new(BoundaryKind::Capture, format!("cannot read fixture: {error}"))
        })?;
    if bytes.len() as u64 > MAX_FIXTURE_BYTES {
        return Err(BoundaryError::new(BoundaryKind::Capture, "fixture is too large"));
    }
    if bytes.is_empty() {
        return Err(BoundaryError::new(BoundaryKind::EmptyRecording, "fixture is empty"));
    }
    Ok(bytes)
}

async fn run_replay(
    fixture_name: &str,
    fixture_dir: &Path,
    id: u64,
    deepgram: &mut Box<dyn TranscriptProvider>,
    groq: &mut Box<dyn TranscriptProvider>,
    validator: &mut dyn TranscriptValidator,
    provider_deadline: Duration,
) -> Result<ReplayOutcome, BoundaryError> {
    let bytes = read_fixture(fixture_dir, fixture_name)?;
    let deepgram_stream = deepgram.start(id)?;
    let groq_stream = match groq.start(id) {
        Ok(stream) => stream,
        Err(error) => {
            // A partial start must not detach the already-started stream: abort
            // it and await the abort under the bounded recovery deadline before
            // the failure becomes observable — the same ownership discipline as
            // the dictation start path.
            match timeout(RECOVERY_ABORT_DEADLINE, deepgram_stream.abort()).await {
                Ok(Ok(())) => {}
                Ok(Err(abort_error)) => eprintln!(
                    "Replay {id}: provider abort failed: {}",
                    abort_error.diagnostic()
                ),
                Err(_) => eprintln!("Replay {id}: provider abort timed out"),
            }
            return Err(error);
        }
    };
    let coordinator = ProviderCoordinator::start(
        provider_deadline,
        RECOVERY_ABORT_DEADLINE,
        ProviderStreams {
            deepgram: deepgram_stream,
            groq: groq_stream,
        },
    );
    replay_capture(CapturedAudio::new(bytes), coordinator, validator).await
}

/// Upper bound the listener waits for the actor's reply to one Toggle before
/// it gives up on that activation and reads the next. It comfortably exceeds a
/// full stop-and-process cycle; exceeding it means the actor is wedged, and the
/// listener logs and moves on rather than blocking future activations forever.
const SHORTCUT_TOGGLE_REPLY_DEADLINE: Duration = Duration::from_secs(60);

/// Binds the Trigger Key through the Global Shortcuts portal and turns each
/// activation into a Toggle. An unavailable portal, a denied or revoked
/// permission, or a stream error retires the listener quietly, clears the
/// displayed binding, and leaves CLI start/stop/toggle fully usable. A portal
/// that leaves the bus (crash/restart) clears the binding immediately and the
/// listener rebinds once a new portal owns the name, so the binding is never
/// stale and a restarted portal ends up rebound. Tests substitute the portal
/// edge by pointing `DBUS_SESSION_BUS_ADDRESS` at a private bus running a
/// controlled portal service — the daemon itself always runs this production
/// listener.
async fn shortcut_listener(actor: mpsc::Sender<ActorMessage>) {
    let mut portal: Box<dyn ShortcutPortal> = Box::new(FedoraShortcutPortal::new());
    'rebind: loop {
        let mut session = match portal.bind().await {
            Ok(session) => session,
            Err(error) => {
                eprintln!("Trigger Key binding is unavailable: {}", error.diagnostic());
                let _ = actor.send(ActorMessage::ShortcutBound(None)).await;
                return;
            }
        };
        if actor
            .send(ActorMessage::ShortcutBound(Some(session.binding())))
            .await
            .is_err()
        {
            return;
        }
        loop {
            match session.next_event().await {
                Ok(voisu_core::ShortcutEvent::Activated) => {
                    let (reply_tx, reply_rx) = oneshot::channel();
                    if actor
                        .send(ActorMessage::Command(Command::Toggle, reply_tx))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    // Await the reply before reading the next activation: the
                    // actor already rejects overlapping Toggles, and processing
                    // them one at a time gives a natural debounce so a burst of
                    // activations cannot spawn overlapping Recordings or
                    // duplicate stop processing.
                    match timeout(SHORTCUT_TOGGLE_REPLY_DEADLINE, reply_rx).await {
                        Ok(Ok(response)) => {
                            eprintln!("Trigger Key activation: {}", response.message);
                        }
                        Ok(Err(_)) => return,
                        Err(_) => {
                            eprintln!("Trigger Key activation timed out awaiting the daemon");
                        }
                    }
                }
                Ok(voisu_core::ShortcutEvent::Revoked) => {
                    eprintln!(
                        "Trigger Key portal ended; start, stop, and toggle remain available"
                    );
                    // Clear the displayed binding: a revoked portal must not
                    // leave `voisu shortcut` claiming a retired Trigger Key.
                    let _ = actor.send(ActorMessage::ShortcutBound(None)).await;
                    return;
                }
                Ok(voisu_core::ShortcutEvent::PortalLost) => {
                    eprintln!(
                        "Trigger Key portal left the bus; binding cleared until it returns"
                    );
                    let _ = actor.send(ActorMessage::ShortcutBound(None)).await;
                    // Keep polling the SAME session: its portal owner watch
                    // stays live and yields PortalRestarted on a new owner.
                }
                Ok(voisu_core::ShortcutEvent::PortalRestarted) => {
                    eprintln!("Trigger Key portal restarted; rebinding the Trigger Key");
                    // The old binding is stale on the new portal either way.
                    let _ = actor.send(ActorMessage::ShortcutBound(None)).await;
                    continue 'rebind;
                }
                Err(error) => {
                    eprintln!(
                        "Trigger Key activation stream failed: {}; start, stop, and toggle \
                         remain available",
                        error.diagnostic()
                    );
                    let _ = actor.send(ActorMessage::ShortcutBound(None)).await;
                    return;
                }
            }
        }
    }
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
    recording_outcome_once: Option<String>,
    fail_abort: bool,
    abort_stall: Duration,
    chunks: u32,
    chunk_delay: Duration,
    deadline_after_chunks: Option<u32>,
}

fn env_millis(name: &str) -> Duration {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or_default()
}

impl ControlledCapture {
    fn from_env() -> Self {
        let chunks = std::env::var("VOISU_TEST_CAPTURE_CHUNKS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(1);
        let chunk_delay = env_millis("VOISU_TEST_CHUNK_DELAY_MS");
        Self {
            fail_finish_once: std::env::var_os("VOISU_TEST_CAPTURE_FINISH_FAILURE").is_some(),
            recording_outcome_once: std::env::var("VOISU_TEST_RECORDING_OUTCOME").ok(),
            fail_abort: std::env::var_os("VOISU_TEST_CAPTURE_ABORT_FAILURE").is_some(),
            abort_stall: env_millis("VOISU_TEST_CAPTURE_ABORT_STALL_MS"),
            chunks,
            chunk_delay,
            deadline_after_chunks: std::env::var("VOISU_TEST_DEADLINE_AFTER_CHUNKS")
                .ok()
                .and_then(|value| value.parse().ok()),
        }
    }
}

impl AudioCapture for ControlledCapture {
    fn begin(&mut self, _recording_id: u64) -> Result<Box<dyn ActiveCapture>, BoundaryError> {
        let fail_finish = std::mem::take(&mut self.fail_finish_once);
        let recording_outcome = self.recording_outcome_once.take();
        Ok(Box::new(ControlledActiveCapture {
            fail_finish,
            recording_outcome,
            fail_abort: self.fail_abort,
            abort_stall: self.abort_stall,
            remaining_chunks: self.chunks,
            chunk_delay: self.chunk_delay,
            deadline_after_chunks: self.deadline_after_chunks,
            chunks_emitted: 0,
        }))
    }
}

struct ControlledActiveCapture {
    fail_finish: bool,
    recording_outcome: Option<String>,
    fail_abort: bool,
    abort_stall: Duration,
    remaining_chunks: u32,
    chunk_delay: Duration,
    deadline_after_chunks: Option<u32>,
    chunks_emitted: u32,
}

impl ActiveCapture for ControlledActiveCapture {
    fn next_chunk(&mut self) -> BoundaryFuture<'_, Option<AudioChunk>> {
        Box::pin(async move {
            // The Recording Deadline is enforced on the next-chunk poll, exactly
            // like the real PipeWire capture: once the configured number of
            // chunks has streamed, a forgotten Recording stops itself with a
            // RecordingDeadline boundary instead of running forever.
            if let Some(limit) = self.deadline_after_chunks
                && self.chunks_emitted >= limit
            {
                return Err(BoundaryError::new(
                    BoundaryKind::RecordingDeadline,
                    "controlled Recording Deadline elapsed",
                ));
            }
            if self.remaining_chunks == 0 {
                std::future::pending::<()>().await;
                unreachable!();
            }
            self.remaining_chunks -= 1;
            self.chunks_emitted += 1;
            if !self.chunk_delay.is_zero() {
                tokio::time::sleep(self.chunk_delay).await;
            }
            Ok(Some(AudioChunk(vec![0])))
        })
    }

    fn finish(&mut self) -> BoundaryFuture<'_, CapturedAudio> {
        Box::pin(async move {
            if let Some(outcome) = self.recording_outcome.take() {
                let kind = match outcome.as_str() {
                    "empty" => BoundaryKind::EmptyRecording,
                    "too-short" => BoundaryKind::TooShortRecording,
                    "silent" => BoundaryKind::SilentRecording,
                    "over-deadline" => BoundaryKind::RecordingDeadline,
                    _ => BoundaryKind::Capture,
                };
                return Err(BoundaryError::new(kind, format!("controlled {outcome} Recording")));
            }
            if self.fail_finish {
                Err(BoundaryError::new(
                    BoundaryKind::Capture,
                    "controlled-secret-capture-detail",
                ))
            } else {
                Ok(CapturedAudio::new(vec![1_u8; 3_200]))
            }
        })
    }

    fn abort(self: Box<Self>) -> BoundaryFuture<'static, ()> {
        let fail_abort = self.fail_abort;
        let abort_stall = self.abort_stall;
        Box::pin(async move {
            if !abort_stall.is_zero() {
                tokio::time::sleep(abort_stall).await;
            }
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
    text: String,
    delay: Duration,
    send_stall: Duration,
    fail_start_once: bool,
    fail_abort: bool,
    fail_complete: bool,
}

impl ControlledProvider {
    fn from_env(provider: Provider) -> Self {
        let provider_delay_name = match provider {
            Provider::Deepgram => "VOISU_TEST_DEEPGRAM_DELAY_MS",
            Provider::Groq => "VOISU_TEST_GROQ_DELAY_MS",
        };
        let delay = if std::env::var_os(provider_delay_name).is_some() {
            env_millis(provider_delay_name)
        } else {
            env_millis("VOISU_TEST_PROVIDER_DELAY_MS")
        };
        let send_stall = env_millis("VOISU_TEST_PROVIDER_SEND_STALL_MS");
        // Only Groq fails its start, so capture and Deepgram are already started
        // when the partial-start-failure abort path is exercised.
        let fail_start_once = provider == Provider::Groq
            && std::env::var_os("VOISU_TEST_PROVIDER_START_FAILURE").is_some();
        let fail_abort = std::env::var_os("VOISU_TEST_PROVIDER_ABORT_FAILURE").is_some();
        let fail_complete = std::env::var("VOISU_TEST_PROVIDER_COMPLETE_FAILURE")
            .ok()
            .is_some_and(|value| value == provider.secret_service_value());
        let transcript_name = match provider {
            Provider::Deepgram => "VOISU_TEST_DEEPGRAM_TRANSCRIPT",
            Provider::Groq => "VOISU_TEST_GROQ_TRANSCRIPT",
        };
        let text = std::env::var(transcript_name)
            .unwrap_or_else(|_| "controlled Source Transcript".to_owned());
        Self {
            provider,
            text,
            delay,
            send_stall,
            fail_start_once,
            fail_abort,
            fail_complete,
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
            text: self.text.clone(),
            delay: self.delay,
            send_stall: self.send_stall,
            fail_abort: self.fail_abort,
            fail_complete: self.fail_complete,
        }))
    }
}

struct ControlledProviderStream {
    provider: Provider,
    text: String,
    delay: Duration,
    send_stall: Duration,
    fail_abort: bool,
    fail_complete: bool,
}

impl ProviderStream for ControlledProviderStream {
    fn provider(&self) -> Provider {
        self.provider
    }

    fn send_audio(&mut self, _chunk: AudioChunk) -> BoundaryFuture<'_, ()> {
        let send_stall = self.send_stall;
        Box::pin(async move {
            if !send_stall.is_zero() {
                tokio::time::sleep(send_stall).await;
            }
            Ok(())
        })
    }

    fn abort(self: Box<Self>) -> BoundaryFuture<'static, ()> {
        Box::pin(async move {
            if self.fail_abort {
                Err(BoundaryError::new(
                    BoundaryKind::Provider,
                    "controlled-provider-abort-detail",
                ))
            } else {
                Ok(())
            }
        })
    }

    fn complete(&mut self, _audio: CapturedAudio) -> BoundaryFuture<'_, SourceTranscript> {
        let provider = self.provider;
        let delay = self.delay;
        let fail_complete = self.fail_complete;
        let text = self.text.clone();
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            if fail_complete {
                return Err(BoundaryError::new(
                    BoundaryKind::Provider,
                    "controlled-provider-completion-detail",
                ));
            }
            Ok(SourceTranscript {
                provider,
                text,
            })
        })
    }
}

struct ControlledValidator {
    pipeline: TranscriptDecisionPipeline<ControlledReconciliationModel>,
}

impl ControlledValidator {
    fn from_env() -> Self {
        let deadline = if std::env::var_os("VOISU_TEST_RECONCILIATION_DEADLINE_MS").is_some() {
            env_millis("VOISU_TEST_RECONCILIATION_DEADLINE_MS").max(Duration::from_millis(1))
        } else {
            RECONCILIATION_DEADLINE
        };
        Self {
            pipeline: TranscriptDecisionPipeline::new(
                ControlledReconciliationModel::from_env(),
                deadline,
            ),
        }
    }
}

impl TranscriptValidator for ControlledValidator {
    fn validate(
        &mut self,
        sources: Vec<SourceTranscript>,
    ) -> BoundaryFuture<'_, TranscriptDecision> {
        self.pipeline.validate(sources)
    }
}

struct ControlledReconciliationModel {
    delay: Duration,
    reconcile_result: Option<String>,
    repair_result: Option<String>,
    fail: bool,
}

impl ControlledReconciliationModel {
    fn from_env() -> Self {
        Self {
            delay: env_millis("VOISU_TEST_RECONCILIATION_DELAY_MS"),
            reconcile_result: std::env::var("VOISU_TEST_RECONCILIATION_RESULT").ok(),
            repair_result: std::env::var("VOISU_TEST_REPAIR_RESULT").ok(),
            fail: std::env::var_os("VOISU_TEST_RECONCILIATION_FAILURE").is_some(),
        }
    }
}

impl ReconciliationModel for ControlledReconciliationModel {
    fn request(
        &mut self,
        kind: ReconciliationKind,
        _sources: Vec<SourceTranscript>,
        _candidate: Option<MergeResult>,
        cancel: Arc<CancelRegistry>,
    ) -> BoundaryFuture<'_, MergeResult> {
        let delay = self.delay;
        let fail = self.fail;
        let result = match kind {
            ReconciliationKind::Reconcile => self.reconcile_result.clone(),
            ReconciliationKind::Repair => self.repair_result.clone(),
        };
        Box::pin(async move {
            if !delay.is_zero() {
                // Honor cancellation during the controlled delay so the
                // pipeline's post-deadline grace await completes promptly, as
                // a cancel-honoring production model would.
                let poll = Duration::from_millis(5);
                let mut waited = Duration::ZERO;
                while waited < delay && !cancel.is_cancelled() {
                    let step = poll.min(delay - waited);
                    tokio::time::sleep(step).await;
                    waited += step;
                }
            }
            if cancel.is_cancelled() {
                return Err(BoundaryError::new(
                    BoundaryKind::Validation,
                    "controlled reconciliation cancelled",
                ));
            }
            if fail {
                return Err(BoundaryError::new(
                    BoundaryKind::Validation,
                    "controlled reconciliation failed",
                ));
            }
            result.map(MergeResult).ok_or_else(|| {
                BoundaryError::new(
                    BoundaryKind::Validation,
                    "controlled reconciliation result missing",
                )
            })
        })
    }
}

struct ControlledDelivery;

impl DeliveryAdapter for ControlledDelivery {
    fn deliver(&mut self, _transcript: Transcript) -> BoundaryFuture<'_, DeliveryOutcome> {
        Box::pin(async {
            Ok(match std::env::var("VOISU_TEST_DELIVERY_FALLBACK") {
                Ok(reason) => DeliveryOutcome::clipboard_fallback(reason),
                Err(_) => DeliveryOutcome::compositor_submitted(),
            })
        })
    }
}
