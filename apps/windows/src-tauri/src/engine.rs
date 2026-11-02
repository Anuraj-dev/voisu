//! The Recording → Transcript engine: the Linux Cloud mode pipeline on
//! Windows. The default mic streams to Deepgram while recording; at stop
//! Groq transcribes the whole Recording, and the two Source Transcripts are
//! reconciled by the same voisu-core decision pipeline the Linux daemon runs.
//!
//! The engine is one actor task. It reports every lifecycle change as a
//! `recording-state` event and the mic level as `audio-level`, which the
//! overlay (W4) listens to. The final Transcript is held in memory only.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use voisu_cloud::{
    CleanupSupervisor, CloudHost, DeepgramProvider, GroqProvider, MergeResultValidator,
};
use voisu_core::{
    ActiveCapture, AudioCapture, BoundaryError, BoundaryFuture, BoundaryKind, CancelRegistry,
    CaptureLimit, Credential, PreparedTranscriptDecision, Provider, ProviderCoordinator,
    ProviderStreams, SecretStore, TranscriptProvider, TranscriptValidator,
    sanitize_source_transcripts,
};

use crate::gesture::Action;
use crate::overlay::{
    self, AUDIO_LEVEL, AudioLevelPayload, RECORDING_STATE, RecordingState as Phase,
    RecordingStatePayload,
};
use crate::platform::capture::{CpalCapture, LevelSink};
use crate::platform::secrets::CredentialManagerStore;

/// Both providers must deliver within this after stop, as on Linux.
const PROVIDER_DEADLINE: Duration = Duration::from_secs(15);
/// Bound on releasing provider streams after a cancel or failure.
const ABORT_DEADLINE: Duration = Duration::from_secs(2);
/// Bound on awaiting dropped streams' cleanup before the next Recording.
const CLEANUP_DRAIN: Duration = Duration::from_secs(2);
/// The language both providers transcribe, the Linux default.
const LANGUAGE: &str = "en";

const KEYS_MISSING: &str = "Add your Groq and Deepgram keys to start dictating.";

/// Tells the trigger that the engine finished or refused a Recording.
pub type EndedSink = Arc<dyn Fn(u64) + Send + Sync>;

/// The Windows host for the cloud pipeline: Credential Manager keys and one
/// cleanup supervisor shared by every provider stream.
#[derive(Clone)]
pub struct WindowsCloudHost {
    cleanup: CleanupSupervisor,
}

impl CloudHost for WindowsCloudHost {
    fn cleanup(&self) -> &CleanupSupervisor {
        &self.cleanup
    }

    fn load_credential(&self, provider: Provider) -> Result<Credential, BoundaryError> {
        CredentialManagerStore.load(provider)
    }

    fn prepare_intent_credential(
        &self,
        cancel: Arc<CancelRegistry>,
    ) -> BoundaryFuture<'static, Credential> {
        Box::pin(async move {
            let cancelled = || {
                BoundaryError::new(
                    BoundaryKind::Validation,
                    "Intent Reconstruction request cancelled",
                )
            };
            if cancel.is_cancelled() {
                return Err(cancelled());
            }
            // Credential Manager reads are local and fast; awaiting the
            // blocking task means no preparation work outlives this future.
            let loaded =
                tokio::task::spawn_blocking(|| CredentialManagerStore.load(Provider::Groq)).await;
            if cancel.is_cancelled() {
                return Err(cancelled());
            }
            match loaded {
                Ok(Ok(credential)) => Ok(credential),
                _ => Err(BoundaryError::new(
                    BoundaryKind::Validation,
                    "Groq credential unavailable for Intent Reconstruction",
                )),
            }
        })
    }
}

/// Engine state the UI reads: the last Transcript, in memory only.
#[derive(Default)]
pub struct EngineState {
    last_transcript: Mutex<Option<String>>,
}

#[tauri::command]
pub fn last_transcript(state: tauri::State<'_, EngineState>) -> Option<String> {
    state.last_transcript.lock().unwrap().clone()
}

/// Starts the engine actor and returns the sender the trigger drives it with.
pub fn start(app: &AppHandle, ended: EndedSink) -> UnboundedSender<Action> {
    app.manage(EngineState::default());
    let (tx, rx) = mpsc::unbounded_channel();
    let engine = Engine {
        app: app.clone(),
        host: WindowsCloudHost {
            cleanup: CleanupSupervisor::new(),
        },
        ended,
    };
    tauri::async_runtime::spawn(engine.run(rx));
    tx
}

struct Engine {
    app: AppHandle,
    host: WindowsCloudHost,
    ended: EndedSink,
}

/// How the capture phase of a Recording ended.
enum Outcome {
    Stop(Option<CaptureLimit>),
    Cancel,
    Failed(BoundaryError),
}

impl Engine {
    async fn run(self, mut actions: UnboundedReceiver<Action>) {
        self.emit_state(Phase::Idle, None);
        while let Some(action) = actions.recv().await {
            match action {
                Action::Start(id) => {
                    self.record(id, &mut actions).await;
                    (self.ended)(id);
                    // Dropped streams finish tearing down before the next
                    // Recording can start.
                    if !self.host.cleanup.drain(CLEANUP_DRAIN).await {
                        tracing::warn!("provider cleanup still running");
                    }
                }
                other => tracing::debug!(?other, "no Recording to act on"),
            }
        }
    }

    async fn record(&self, id: u64, actions: &mut UnboundedReceiver<Action>) {
        // A tray preview still playing must not hide or feed this session's
        // capsule.
        overlay::end_preview();
        if !crate::keys_ready() {
            tracing::info!(id, "Recording refused: keys missing");
            self.emit_state(Phase::Error, Some(KEYS_MISSING.to_owned()));
            crate::show_settings(&self.app);
            return;
        }
        self.emit_state(Phase::Recording, None);

        let levels = self.level_sink();
        let begun = tokio::task::spawn_blocking(move || CpalCapture::new(levels).begin(id)).await;
        let mut capture = match begun {
            Ok(Ok(capture)) => capture,
            Ok(Err(error)) => return self.fail(id, &error),
            Err(join) => {
                let error = BoundaryError::new(
                    BoundaryKind::Capture,
                    format!("capture start failed: {join}"),
                );
                return self.fail(id, &error);
            }
        };
        let streams = match self.start_providers(id) {
            Ok(streams) => streams,
            Err(error) => {
                let _ = capture.abort().await;
                return self.fail(id, &error);
            }
        };
        let mut providers = ProviderCoordinator::start(PROVIDER_DEADLINE, ABORT_DEADLINE, streams);
        let outcome = self.pump(id, &mut capture, &mut providers, actions).await;

        let limit = match outcome {
            Outcome::Stop(limit) => limit,
            Outcome::Cancel => {
                tracing::info!(id, "Recording cancelled");
                abort(capture, providers).await;
                return self.emit_state(Phase::Idle, None);
            }
            Outcome::Failed(error) => {
                abort(capture, providers).await;
                return self.fail(id, &error);
            }
        };
        self.emit_state(Phase::Processing, None);
        let audio = match capture.finish().await {
            Ok(audio) => audio,
            Err(error) => {
                abort(capture, providers).await;
                return self.fail(id, &error);
            }
        };
        let audio = match limit {
            Some(limit) => audio.with_truncation(limit),
            None => audio,
        };
        match self.decide(providers, audio).await {
            Ok(text) => {
                tracing::info!(id, chars = text.chars().count(), "Transcript ready");
                *self
                    .app
                    .state::<EngineState>()
                    .last_transcript
                    .lock()
                    .unwrap() = Some(text);
                self.emit_state(Phase::Idle, None);
                let _ = self.app.emit(crate::STATE_CHANGED, ());
            }
            Err(error) => self.fail(id, &error),
        }
    }

    /// Streams capture chunks to Deepgram until the trigger stops or cancels
    /// the Recording, the capture ends, or something fails. Mirrors the
    /// Linux `capture_pump`: actions are always observed, even while a chunk
    /// send is stalled.
    async fn pump(
        &self,
        id: u64,
        capture: &mut Box<dyn ActiveCapture>,
        providers: &mut ProviderCoordinator,
        actions: &mut UnboundedReceiver<Action>,
    ) -> Outcome {
        loop {
            let chunk = tokio::select! {
                biased;
                action = actions.recv() => match self.on_action(id, action) {
                    Some(outcome) => return outcome,
                    None => continue,
                },
                chunk = capture.next_chunk() => chunk,
            };
            let chunk = match chunk {
                Ok(Some(chunk)) => chunk,
                // The capture hit its byte cap.
                Ok(None) => return Outcome::Stop(None),
                Err(error) if error.kind() == BoundaryKind::RecordingDeadline => {
                    tracing::info!(id, "Recording Deadline reached");
                    return Outcome::Stop(Some(CaptureLimit::RecordingDeadline));
                }
                Err(error) => return Outcome::Failed(error),
            };
            let send = providers.stream_audio(chunk);
            tokio::pin!(send);
            loop {
                tokio::select! {
                    biased;
                    action = actions.recv() => {
                        if let Some(outcome) = self.on_action(id, action) {
                            return outcome;
                        }
                    }
                    sent = &mut send => match sent {
                        Ok(()) => break,
                        Err(error) => return Outcome::Failed(error),
                    },
                }
            }
        }
    }

    /// Applies a trigger action during Recording `id`. Returns how the
    /// capture phase ends, if this action ends it.
    fn on_action(&self, id: u64, action: Option<Action>) -> Option<Outcome> {
        match action {
            // The trigger is gone: nothing can stop this Recording.
            None => Some(Outcome::Cancel),
            Some(Action::HandsFree(current)) if current == id => {
                self.emit_state(Phase::HandsFree, None);
                None
            }
            Some(Action::Stop(current)) if current == id => Some(Outcome::Stop(None)),
            Some(Action::Cancel(current)) if current == id => Some(Outcome::Cancel),
            Some(Action::Start(other)) => {
                // One Recording at a time.
                (self.ended)(other);
                None
            }
            Some(stale) => {
                tracing::debug!(?stale, id, "stale trigger action");
                None
            }
        }
    }

    fn start_providers(&self, id: u64) -> Result<ProviderStreams, BoundaryError> {
        let mut deepgram = DeepgramProvider::with_keyterms_and_language(
            self.host.clone(),
            Vec::new(),
            LANGUAGE.to_owned(),
        );
        let mut groq = GroqProvider::with_prompt_and_language(
            self.host.clone(),
            String::new(),
            LANGUAGE.to_owned(),
        );
        let deepgram = deepgram.start(id)?;
        let groq = match groq.start(id) {
            Ok(groq) => groq,
            Err(error) => {
                tauri::async_runtime::spawn(deepgram.abort());
                return Err(error);
            }
        };
        Ok(ProviderStreams { deepgram, groq })
    }

    /// The Linux live decision path without the Linux-only formatting stages:
    /// complete both providers, sanitize, validate, and reconstruct when the
    /// validator asks for it.
    async fn decide(
        &self,
        providers: ProviderCoordinator,
        audio: voisu_core::CapturedAudio,
    ) -> Result<String, BoundaryError> {
        let completed = providers.complete_with_timings(audio).await?;
        for failure in &completed.provider_failures {
            tracing::warn!(?failure, "provider did not contribute");
        }
        let mut validator = MergeResultValidator::intent_reconstruction(self.host.clone());
        validator.set_word_confidences(completed.word_confidences);
        let sources = sanitize_source_transcripts(completed.sources);
        let decision = match validator.prepare(sources).await? {
            PreparedTranscriptDecision::Ready(decision) => decision,
            PreparedTranscriptDecision::Reconstruct(attempt) => {
                validator.reconstruct(attempt).await?
            }
        };
        Ok(decision.transcript.0)
    }

    fn fail(&self, id: u64, error: &BoundaryError) {
        // Diagnostics never carry Transcript text or key material.
        tracing::warn!(id, kind = ?error.kind(), diagnostic = error.diagnostic(), "Recording failed");
        match error.kind() {
            // Nothing worth reporting: an accidental or silent press.
            BoundaryKind::EmptyRecording
            | BoundaryKind::TooShortRecording
            | BoundaryKind::SilentRecording => self.emit_state(Phase::Idle, None),
            _ => self.emit_state(Phase::Error, Some(error.public_message().to_owned())),
        }
    }

    fn emit_state(&self, state: Phase, message: Option<String>) {
        if let Some(tray) = self.app.state::<crate::AppState>().tray.get() {
            match state {
                Phase::Recording | Phase::HandsFree => tray.set_status("Voisu: recording…"),
                Phase::Processing => tray.set_status("Voisu: transcribing…"),
                Phase::Idle | Phase::Error => tray.set_ready(crate::keys_ready()),
            }
        }
        let _ = self
            .app
            .emit(RECORDING_STATE, RecordingStatePayload { state, message });
    }

    fn level_sink(&self) -> LevelSink {
        let app = self.app.clone();
        Arc::new(move |level| {
            let _ = app.emit(AUDIO_LEVEL, AudioLevelPayload { level });
        })
    }
}

async fn abort(capture: Box<dyn ActiveCapture>, providers: ProviderCoordinator) {
    let (capture, providers) = tokio::join!(capture.abort(), providers.abort());
    if let Err(error) = capture.and(providers) {
        tracing::warn!(diagnostic = error.diagnostic(), "Recording cleanup failed");
    }
}
