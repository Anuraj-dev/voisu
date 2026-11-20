//! The Recording → Transcript engine: the Linux Cloud mode pipeline on
//! Windows. The default mic streams to Deepgram while recording; at stop
//! Groq transcribes the whole Recording, and the two Source Transcripts are
//! reconciled by the same voisu-core decision pipeline the Linux daemon runs,
//! then formatted locally exactly as Linux does.
//!
//! The engine is one actor task. It reports every lifecycle change as a
//! `recording-state` event and the mic level as `audio-level`, which the
//! overlay (W4) listens to. The final Transcript is pasted into the window
//! that was focused when the Recording started (or only copied, with
//! auto-paste off), and held in memory only so "Last transcript" can copy it
//! when the paste does not land.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use voisu_cloud::dpr_local::render_dpr_local;
use voisu_cloud::{
    CleanupSupervisor, CloudHost, DeepgramProvider, GroqProvider, MergeResultValidator,
};
use voisu_core::{
    ActiveCapture, BoundaryError, BoundaryFuture, BoundaryKind, CancelRegistry, CaptureLimit,
    CapturedAudio, Credential, DEFAULT_RENDERING_POLICY, EnglishEligibilityOutcome,
    PreparedTranscriptDecision, Provider, ProviderCoordinator, ProviderStreams,
    ResolvedRecordingLanguages, SecretStore, SourceTranscript, Tone, TranscriptProvider,
    TranscriptValidator, apply_tone, sanitize_source_transcripts,
};

use crate::gesture::Action;
use crate::overlay::{
    self, AUDIO_LEVEL, AudioLevelPayload, RECORDING_STATE, RecordingState as Phase,
    RecordingStatePayload,
};
use crate::platform::capture::{CaptureStartup, CpalCapture, LevelSink};
use crate::platform::delivery::{self, DeliveryTarget};
use crate::platform::secrets::CredentialManagerStore;
use crate::settings::Settings;
use crate::vocabulary;

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
    recording_keys: Option<RecordingKeys>,
}

#[derive(Clone)]
struct RecordingKeys {
    deepgram: Credential,
    groq: Credential,
}

impl RecordingKeys {
    fn load() -> Result<Self, BoundaryError> {
        let mut store = CredentialManagerStore;
        Ok(Self {
            groq: store.load(Provider::Groq)?,
            deepgram: store.load(Provider::Deepgram)?,
        })
    }
}

impl CloudHost for WindowsCloudHost {
    fn cleanup(&self) -> &CleanupSupervisor {
        &self.cleanup
    }

    fn load_credential(&self, provider: Provider) -> Result<Credential, BoundaryError> {
        match (&self.recording_keys, provider) {
            (Some(keys), Provider::Deepgram) => Ok(keys.deepgram.clone()),
            (Some(keys), Provider::Groq) => Ok(keys.groq.clone()),
            (None, provider) => CredentialManagerStore.load(provider),
        }
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

/// Engine state the UI reads, in memory only: the last Transcript and the
/// phase of the dictation under way.
#[derive(Default)]
pub struct EngineState {
    last_transcript: Mutex<Option<String>>,
    /// The phase under way and when its Recording started; `None` once it
    /// ends.
    phase: Mutex<Option<(Phase, Instant)>>,
}

/// The dictation under way, so a window opened during one starts from it.
#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhaseNow {
    pub state: Phase,
    /// Milliseconds since the Recording started.
    pub elapsed_ms: u64,
}

pub fn current_phase(app: &AppHandle) -> Option<PhaseNow> {
    let engine = app.try_state::<EngineState>()?;
    let (state, since) = (*engine.phase.lock().unwrap())?;
    Some(PhaseNow {
        state,
        elapsed_ms: u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX),
    })
}

/// The phase under way after `state` is reported. Hands-free and processing
/// continue the Recording already running; a terminal state ends it.
fn next_phase(
    prev: Option<(Phase, Instant)>,
    state: Phase,
    now: Instant,
) -> Option<(Phase, Instant)> {
    match state {
        Phase::Idle | Phase::Done | Phase::Copied | Phase::Error => None,
        Phase::Recording => Some((state, now)),
        Phase::HandsFree | Phase::Processing => Some((state, prev.map_or(now, |(_, since)| since))),
    }
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
            recording_keys: None,
        },
        microphone: CpalCapture::new(),
        ended,
    };
    tauri::async_runtime::spawn(engine.run(rx));
    tx
}

struct Engine {
    app: AppHandle,
    host: WindowsCloudHost,
    microphone: CpalCapture,
    ended: EndedSink,
}

/// How the capture phase of a Recording ended.
enum Outcome {
    Stop(Option<CaptureLimit>),
    Cancel,
    Failed(BoundaryError),
}

enum RecordingAudio {
    Live(Box<dyn ActiveCapture>),
    Stopped(CapturedAudio),
}

/// What the capture phase leaves for the processing phase.
enum Captured {
    /// Nothing left to do; the state is already reported.
    Done,
    Process {
        audio: RecordingAudio,
        providers: ProviderCoordinator,
        limit: Option<CaptureLimit>,
        target: DeliveryTarget,
        /// The boost words this Recording's providers started with, so
        /// reconcile sees the same list.
        vocabulary: Vec<String>,
        /// The Tone as this Recording started.
        tone: Tone,
    },
    /// Cancelled (`None`) or failed: release the mic and provider streams.
    Abort {
        capture: Box<dyn ActiveCapture>,
        providers: ProviderCoordinator,
        error: Option<BoundaryError>,
    },
}

impl Engine {
    async fn run(self, mut actions: UnboundedReceiver<Action>) {
        self.emit_state(Phase::Idle, None);
        while let Some(action) = actions.recv().await {
            match action {
                Action::Start { id, hotkey_at } => {
                    let captured = self.record(id, hotkey_at, &mut actions).await;
                    // The gesture is free as soon as capture ends, so Esc and
                    // the chord are not swallowed while this one processes.
                    (self.ended)(id);
                    while_busy(self.settle(id, captured), &mut actions, &self.ended).await;
                    // Dropped streams finish tearing down before the next
                    // Recording can start.
                    let drain = self.host.cleanup.drain(CLEANUP_DRAIN);
                    if !while_busy(drain, &mut actions, &self.ended).await {
                        tracing::warn!("provider cleanup still running");
                    }
                }
                other => tracing::debug!(?other, "no Recording to act on"),
            }
        }
    }

    /// The capture phase: opens the mic and providers and streams until the
    /// trigger or the capture ends it.
    async fn record(
        &self,
        id: u64,
        hotkey_at: Instant,
        actions: &mut UnboundedReceiver<Action>,
    ) -> Captured {
        // First, before any Voisu window (settings on missing keys) can take
        // focus: the Transcript goes where the user was typing.
        let target = DeliveryTarget::capture();
        // A tray preview still playing must not hide or feed this session's
        // capsule.
        overlay::end_preview();
        let startup = Arc::new(CaptureStartup::default());
        let mut providers_started = None;
        let mut trigger = StartupTrigger::new(id, actions, &self.ended);
        let keys = tokio::select! {
            biased;
            () = trigger.until_end() => None,
            result = load_recording_keys() => Some(result),
        };
        let keys = match keys {
            Some(Some(keys)) => keys,
            Some(None) => {
                tracing::info!(id, "Recording refused: keys missing");
                self.emit_state(Phase::Error, Some(KEYS_MISSING.to_owned()));
                crate::show_settings(&self.app);
                startup.log(id, hotkey_at, None);
                return Captured::Done;
            }
            None => {
                startup.log(id, hotkey_at, None);
                return Captured::Done;
            }
        };
        let started = start_recording(
            self.microphone
                .begin(id, self.level_sink(), Arc::clone(&startup)),
            async {
                let settings = self.settings();
                let vocabulary = vocabulary::normalize(&settings.boost_words);
                let streams = self.start_providers(id, &vocabulary, keys)?;
                let providers =
                    ProviderCoordinator::start(PROVIDER_DEADLINE, ABORT_DEADLINE, streams);
                providers_started = Some(Instant::now());
                Ok((providers, vocabulary, settings.tone))
            },
            |phase| self.emit_state(phase, None),
            &mut trigger,
        )
        .await;
        startup.log(id, hotkey_at, providers_started);
        let (audio, (mut providers, vocabulary, tone)) = match started {
            Ok(Some(started)) => started,
            Ok(None) => {
                self.emit_state(Phase::Idle, None);
                return Captured::Done;
            }
            Err(error) => {
                self.fail(id, &error);
                return Captured::Done;
            }
        };
        let mut capture = match audio {
            RecordingAudio::Live(capture) => capture,
            RecordingAudio::Stopped(_) => {
                return Captured::Process {
                    audio,
                    providers,
                    limit: None,
                    target,
                    vocabulary,
                    tone,
                };
            }
        };
        match self.pump(id, &mut capture, &mut providers, actions).await {
            Outcome::Stop(limit) => Captured::Process {
                audio: RecordingAudio::Live(capture),
                providers,
                limit,
                target,
                vocabulary,
                tone,
            },
            Outcome::Cancel => {
                tracing::info!(id, "Recording cancelled");
                Captured::Abort {
                    capture,
                    providers,
                    error: None,
                }
            }
            Outcome::Failed(error) => Captured::Abort {
                capture,
                providers,
                error: Some(error),
            },
        }
    }

    /// The processing phase: a Transcript from a stopped Recording, or the
    /// teardown of a cancelled or failed one.
    async fn settle(&self, id: u64, captured: Captured) {
        let (audio, providers, limit, target, vocabulary, tone) = match captured {
            Captured::Done => return,
            Captured::Abort {
                capture,
                providers,
                error,
            } => {
                abort(capture, providers).await;
                return match error {
                    Some(error) => self.fail(id, &error),
                    None => self.emit_state(Phase::Idle, None),
                };
            }
            Captured::Process {
                audio,
                providers,
                limit,
                target,
                vocabulary,
                tone,
            } => (audio, providers, limit, target, vocabulary, tone),
        };
        let audio = match audio {
            RecordingAudio::Live(mut capture) => {
                self.emit_state(Phase::Processing, None);
                match capture.finish().await {
                    Ok(audio) => audio,
                    Err(error) => {
                        abort(capture, providers).await;
                        return self.fail(id, &error);
                    }
                }
            }
            RecordingAudio::Stopped(audio) => audio,
        };
        let audio = match limit {
            Some(limit) => audio.with_truncation(limit),
            None => audio,
        };
        match self.decide(providers, audio, vocabulary).await {
            Ok(text) => {
                let text = apply_tone(&text, tone);
                tracing::info!(id, chars = text.chars().count(), "Transcript ready");
                // Stored first: "Last transcript" + Copy is the fallback
                // whenever the paste does not land.
                *self
                    .app
                    .state::<EngineState>()
                    .last_transcript
                    .lock()
                    .unwrap() = Some(text.clone());
                let _ = self.app.emit(crate::STATE_CHANGED, ());
                if self.deliver(id, target, text).await {
                    // On macOS the result can arrive before clipboard restore.
                    // This hook counts toward the star prompt and may send a
                    // notification. It leaves the clipboard and focus alone.
                    crate::dictation_delivered(&self.app);
                }
            }
            Err(error) => self.fail(id, &error),
        }
    }

    /// Pastes the Transcript into `target`, or with auto-paste off only puts
    /// it on the clipboard. Delivery blocks and pumps its own messages, so it
    /// runs on a blocking thread, which on macOS may keep the user's
    /// clipboard parked a little after the result arrives here. Returns
    /// whether the Transcript was delivered: pasted, or left on the
    /// clipboard for the user to paste.
    async fn deliver(&self, id: u64, target: DeliveryTarget, text: String) -> bool {
        if !self.settings().auto_paste {
            let copied = tokio::task::spawn_blocking(move || delivery::copy(&text)).await;
            let message = match copied {
                Ok(Ok(())) => {
                    self.emit_state(Phase::Done, Some(delivery::COPIED.to_owned()));
                    return true;
                }
                Ok(Err(error)) => {
                    tracing::warn!(id, %error, "copy failed");
                    Some(error.to_string())
                }
                Err(_) => Some("Couldn't copy the Transcript".to_owned()),
            };
            return self.delivered(message);
        }
        let (settled, result) = tokio::sync::oneshot::channel();
        tokio::task::spawn_blocking(move || {
            delivery::deliver(&target, &text, |result| {
                let _ = settled.send(result);
            });
        });
        match result.await {
            Ok(result) => {
                let (state, message) = delivery::overlay_state(&result);
                self.emit_state(state, message);
                matches!(state, Phase::Done | Phase::Copied)
            }
            Err(_) => {
                tracing::warn!(id, "delivery task ended without a result");
                self.delivered(Some("Couldn't paste the Transcript".to_owned()))
            }
        }
    }

    /// Success unless there is something to tell the user; returns whether
    /// it was a success.
    fn delivered(&self, message: Option<String>) -> bool {
        match message {
            None => {
                self.emit_state(Phase::Done, None);
                true
            }
            Some(message) => {
                self.emit_state(Phase::Error, Some(message));
                false
            }
        }
    }

    fn settings(&self) -> Settings {
        let state = self.app.state::<crate::AppState>();
        let settings = state.settings.lock().unwrap();
        settings.clone()
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
        match recording_action(id, action, &self.ended)? {
            TriggerAction::HandsFree => {
                self.emit_state(Phase::HandsFree, None);
                None
            }
            TriggerAction::Stop => Some(Outcome::Stop(None)),
            TriggerAction::Cancel => Some(Outcome::Cancel),
        }
    }

    /// Starts both providers biased toward `vocabulary`, within the same
    /// keyterm and prompt budgets as Linux.
    fn start_providers(
        &self,
        id: u64,
        vocabulary: &[String],
        keys: RecordingKeys,
    ) -> Result<ProviderStreams, BoundaryError> {
        let host = WindowsCloudHost {
            cleanup: self.host.cleanup.clone(),
            recording_keys: Some(keys),
        };
        let mut deepgram = DeepgramProvider::with_keyterms_and_language(
            host.clone(),
            vocabulary::deepgram_keyterms(vocabulary),
            LANGUAGE.to_owned(),
        );
        let mut groq = GroqProvider::with_prompt_and_language(
            host,
            vocabulary::whisper_prompt(vocabulary),
            LANGUAGE.to_owned(),
        );
        let deepgram = deepgram.start(id)?;
        // On partial failure, Deepgram's Drop cancels synchronously and
        // hands its WebSocket task to the engine's cleanup supervisor.
        let groq = groq.start(id)?;
        Ok(ProviderStreams { deepgram, groq })
    }

    /// The Linux live decision path with local formatting and no cloud
    /// formatter: complete both providers, sanitize, validate, reconstruct
    /// when the validator asks for it, then format locally (see
    /// [`format_locally`]). The boost words are both the user vocabulary and
    /// the whole dictionary: Windows has no built-in terms.
    async fn decide(
        &self,
        providers: ProviderCoordinator,
        audio: voisu_core::CapturedAudio,
        vocabulary: Vec<String>,
    ) -> Result<String, BoundaryError> {
        let completed = providers.complete_with_timings(audio).await?;
        for failure in &completed.provider_failures {
            tracing::warn!(?failure, "provider did not contribute");
        }
        let mut validator = MergeResultValidator::intent_reconstruction(self.host.clone());
        validator.set_word_confidences(completed.word_confidences);
        validator.set_user_vocabulary(vocabulary.clone());
        validator.set_dictionary_terms(vocabulary.clone());
        let sources = sanitize_source_transcripts(completed.sources);
        let decision = match validator.prepare(sources.clone()).await? {
            PreparedTranscriptDecision::Ready(decision) => decision,
            PreparedTranscriptDecision::Reconstruct(attempt) => {
                validator.reconstruct(attempt).await?
            }
        };
        Ok(format_locally(decision.transcript.0, &sources, &vocabulary))
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
        {
            let engine = self.app.state::<EngineState>();
            let mut phase = engine.phase.lock().unwrap();
            *phase = next_phase(*phase, state, Instant::now());
        }
        if let Some(tray) = self.app.state::<crate::AppState>().tray.get() {
            match state {
                Phase::Recording | Phase::HandsFree => tray.set_status("Voisu: recording…"),
                Phase::Processing => tray.set_status("Voisu: transcribing…"),
                Phase::Idle | Phase::Done | Phase::Copied | Phase::Error => {
                    tray.set_ready(crate::keys_ready())
                }
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

async fn load_recording_keys() -> Option<RecordingKeys> {
    tokio::task::spawn_blocking(RecordingKeys::load)
        .await
        .ok()?
        .ok()
}

enum TriggerAction {
    Stop,
    Cancel,
    HandsFree,
}

fn recording_action(id: u64, action: Option<Action>, ended: &EndedSink) -> Option<TriggerAction> {
    match action {
        None => Some(TriggerAction::Cancel),
        Some(Action::HandsFree(current)) if current == id => Some(TriggerAction::HandsFree),
        Some(Action::Stop(current)) if current == id => Some(TriggerAction::Stop),
        Some(Action::Cancel(current)) if current == id => Some(TriggerAction::Cancel),
        Some(Action::Start { id: other, .. }) => {
            ended(other);
            None
        }
        Some(stale) => {
            tracing::debug!(?stale, id, "stale trigger action");
            None
        }
    }
}

struct StartupTrigger<'a> {
    id: u64,
    actions: &'a mut UnboundedReceiver<Action>,
    ended: &'a EndedSink,
    hands_free: bool,
}

impl<'a> StartupTrigger<'a> {
    fn new(id: u64, actions: &'a mut UnboundedReceiver<Action>, ended: &'a EndedSink) -> Self {
        Self {
            id,
            actions,
            ended,
            hands_free: false,
        }
    }

    async fn next(&mut self) -> TriggerAction {
        loop {
            if let Some(action) = recording_action(self.id, self.actions.recv().await, self.ended) {
                if matches!(action, TriggerAction::HandsFree) {
                    self.hands_free = true;
                }
                return action;
            }
        }
    }

    async fn until_end(&mut self) {
        while matches!(self.next().await, TriggerAction::HandsFree) {}
    }
}

/// Provider setup starts as soon as the capture worker is in flight. Keep
/// polling the Trigger Key while either branch is pending.
async fn start_recording<P>(
    capture: impl Future<Output = Result<Box<dyn ActiveCapture>, BoundaryError>>,
    providers: impl Future<Output = Result<P, BoundaryError>>,
    mut emit: impl FnMut(Phase),
    trigger: &mut StartupTrigger<'_>,
) -> Result<Option<(RecordingAudio, P)>, BoundaryError> {
    tokio::pin!(capture, providers);
    let mut opened: Option<Box<dyn ActiveCapture>> = None;
    let mut ready_providers = None;
    loop {
        tokio::select! {
            biased;
            action = trigger.next() => match action {
                TriggerAction::Cancel => return Ok(None),
                TriggerAction::Stop => {
                    let Some(mut capture) = opened else { return Ok(None) };
                    // Stop the mic before waiting for providers. Their completion
                    // path sends the full PCM when no chunks were streamed live.
                    let finish = async {
                        let audio = capture.finish().await?;
                        emit(Phase::Processing);
                        let providers = match ready_providers {
                            Some(providers) => providers,
                            None => providers.await?,
                        };
                        Ok(Some((RecordingAudio::Stopped(audio), providers)))
                    };
                    return while_busy(finish, trigger.actions, trigger.ended).await;
                }
                TriggerAction::HandsFree if opened.is_some() => emit(Phase::HandsFree),
                TriggerAction::HandsFree => {}
            },
            result = &mut capture, if opened.is_none() => {
                opened = Some(result?);
                emit(Phase::Recording);
                if trigger.hands_free {
                    emit(Phase::HandsFree);
                }
            },
            result = &mut providers, if ready_providers.is_none() => ready_providers = Some(result?),
        }
        if opened.is_some() && ready_providers.is_some() {
            return Ok(opened.map(RecordingAudio::Live).zip(ready_providers));
        }
    }
}

/// Local formatting, exactly as the Linux daemon delivers it with
/// `VOISU_ENABLE_DPR` on and no cloud formatter: spoken "new line" and "new
/// paragraph" become line breaks, and spoken marks, quotes, and first/second
/// lists are organized, in the default rendering policy (Windows has no
/// setting for it). Like Linux, only an English-eligible Recording is
/// formatted; anything else, or no Source Transcript with text, keeps the
/// validated Transcript as it is.
fn format_locally(
    validated: String,
    sources: &[SourceTranscript],
    vocabulary: &[String],
) -> String {
    let languages = ResolvedRecordingLanguages::new(vec![
        (Provider::Groq, LANGUAGE.to_owned()),
        (Provider::Deepgram, LANGUAGE.to_owned()),
    ]);
    if languages.english_eligibility() != EnglishEligibilityOutcome::Eligible {
        return validated;
    }
    render_dpr_local(&validated, sources, vocabulary, DEFAULT_RENDERING_POLICY).unwrap_or(validated)
}

/// Runs `work` while refusing every trigger action, so a chord pressed
/// during processing is ended at once instead of replaying as a late,
/// empty Recording once this one finishes.
async fn while_busy<F: Future>(
    work: F,
    actions: &mut UnboundedReceiver<Action>,
    ended: &EndedSink,
) -> F::Output {
    tokio::pin!(work);
    loop {
        tokio::select! {
            biased;
            output = &mut work => return output,
            Some(action) = actions.recv() => {
                if let Action::Start { id, .. } = action {
                    tracing::debug!(id, "Recording refused: still processing");
                    ended(id);
                }
            }
        }
    }
}

async fn abort(capture: Box<dyn ActiveCapture>, providers: ProviderCoordinator) {
    let (capture, providers) = tokio::join!(capture.abort(), providers.abort());
    if let Err(error) = capture.and(providers) {
        tracing::warn!(diagnostic = error.diagnostic(), "Recording cleanup failed");
    }
}

#[cfg(test)]
mod tests {
    use std::future::poll_fn;
    use std::task::Poll;

    use super::*;

    #[test]
    fn a_recording_keeps_its_start_until_it_ends() {
        let start = Instant::now();
        let later = start + Duration::from_secs(5);
        let recording = next_phase(None, Phase::Recording, start);
        let hands_free = next_phase(recording, Phase::HandsFree, later);
        assert_eq!(hands_free, Some((Phase::HandsFree, start)));
        let processing = next_phase(hands_free, Phase::Processing, later);
        assert_eq!(processing, Some((Phase::Processing, start)));
        for end in [Phase::Idle, Phase::Done, Phase::Copied, Phase::Error] {
            assert_eq!(next_phase(processing, end, later), None);
        }
        // The next Recording starts its own clock.
        assert_eq!(
            next_phase(processing, Phase::Recording, later),
            Some((Phase::Recording, later))
        );
    }

    #[tokio::test]
    async fn listening_is_reported_only_after_audio_arrives_even_if_providers_start_first() {
        let (_send, mut actions) = mpsc::unbounded_channel();
        let ended: EndedSink = Arc::new(|_| {});
        let mut trigger = StartupTrigger::new(1, &mut actions, &ended);
        let (first_audio, audio) = tokio::sync::oneshot::channel();
        let events = std::cell::RefCell::new(Vec::new());
        let start = start_recording(
            async {
                audio.await.unwrap();
                Ok(startup_mic(None))
            },
            async {
                events.borrow_mut().push("providers started");
                Ok("providers")
            },
            |phase| {
                assert_eq!(phase, Phase::Recording);
                events.borrow_mut().push("recording");
            },
            &mut trigger,
        );
        tokio::pin!(start);
        poll_fn(|cx| {
            assert!(start.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert_eq!(*events.borrow(), ["providers started"]);

        first_audio.send(()).unwrap();
        assert!(matches!(
            start.await.unwrap(),
            Some((RecordingAudio::Live(_), "providers"))
        ));
        assert_eq!(*events.borrow(), ["providers started", "recording"]);
    }

    #[tokio::test]
    async fn listening_does_not_wait_for_provider_setup() {
        let (_send, mut actions) = mpsc::unbounded_channel();
        let ended: EndedSink = Arc::new(|_| {});
        let mut trigger = StartupTrigger::new(1, &mut actions, &ended);
        let (providers_ready, providers) = tokio::sync::oneshot::channel();
        let listening = std::cell::Cell::new(false);
        let start = start_recording(
            async { Ok(startup_mic(None)) },
            async { Ok(providers.await.unwrap()) },
            |phase| {
                assert_eq!(phase, Phase::Recording);
                listening.set(true);
            },
            &mut trigger,
        );
        tokio::pin!(start);
        poll_fn(|cx| {
            assert!(start.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert!(listening.get());

        providers_ready.send("providers").unwrap();
        assert!(matches!(
            start.await.unwrap(),
            Some((RecordingAudio::Live(_), "providers"))
        ));
    }

    #[tokio::test]
    async fn stop_or_cancel_during_startup_ends_quietly() {
        for action in [Action::Stop(1), Action::Cancel(1)] {
            let (send, mut actions) = mpsc::unbounded_channel();
            let ended: EndedSink = Arc::new(|_| {});
            let mut trigger = StartupTrigger::new(1, &mut actions, &ended);
            let (callback, audio) = tokio::sync::oneshot::channel();
            let start = start_recording(
                async {
                    audio.await.unwrap();
                    Ok(startup_mic(None))
                },
                std::future::pending::<Result<(), BoundaryError>>(),
                |_| panic!("bars shown for {action:?}"),
                &mut trigger,
            );
            let mut start = Box::pin(start);
            poll_fn(|cx| {
                assert!(start.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            send.send(action).unwrap();
            // Both the action and audio can be ready before the next poll.
            callback.send(()).unwrap();
            poll_fn(|cx| {
                assert!(matches!(start.as_mut().poll(cx), Poll::Ready(Ok(None))));
                Poll::Ready(())
            })
            .await;
        }
    }

    #[tokio::test]
    async fn stop_after_first_audio_finishes_the_mic_before_providers_are_ready() {
        let (send, mut actions) = mpsc::unbounded_channel();
        let ended: EndedSink = Arc::new(|_| {});
        let mut trigger = StartupTrigger::new(1, &mut actions, &ended);
        let (providers_ready, providers) = tokio::sync::oneshot::channel();
        let (finished, mut stopped) = tokio::sync::oneshot::channel();
        let phases = std::cell::RefCell::new(Vec::new());
        let start = start_recording(
            async { Ok(startup_mic(Some(finished))) },
            async {
                providers.await.unwrap();
                Ok(())
            },
            |phase| phases.borrow_mut().push(phase),
            &mut trigger,
        );
        tokio::pin!(start);
        poll_fn(|cx| {
            assert!(start.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        send.send(Action::Stop(1)).unwrap();
        poll_fn(|cx| {
            assert!(start.as_mut().poll(cx).is_pending());
            assert_eq!(stopped.try_recv(), Ok(()));
            assert_eq!(*phases.borrow(), [Phase::Recording, Phase::Processing]);
            Poll::Ready(())
        })
        .await;
        providers_ready.send(()).unwrap();
        let Some((RecordingAudio::Stopped(audio), ())) = start.await.unwrap() else {
            panic!("expected buffered audio from the stopped mic");
        };
        assert_eq!(audio.pcm_s16le_mono_16khz(), [1, 2]);
    }

    fn startup_mic(finished: Option<tokio::sync::oneshot::Sender<()>>) -> Box<dyn ActiveCapture> {
        struct StartupMic(Option<tokio::sync::oneshot::Sender<()>>);

        impl ActiveCapture for StartupMic {
            fn deadline_clock(&self) -> voisu_core::DeadlineClock {
                unreachable!("the capture pump has not started")
            }

            fn next_chunk(&mut self) -> BoundaryFuture<'_, Option<voisu_core::AudioChunk>> {
                unreachable!("the capture pump has not started")
            }

            fn finish(&mut self) -> BoundaryFuture<'_, CapturedAudio> {
                Box::pin(async move {
                    self.0.take().unwrap().send(()).unwrap();
                    Ok(CapturedAudio::new(vec![1, 2]))
                })
            }

            fn abort(self: Box<Self>) -> BoundaryFuture<'static, ()> {
                unreachable!("startup drops the capture guard")
            }
        }

        Box::new(StartupMic(finished))
    }

    fn both_providers_heard(text: &str) -> Vec<SourceTranscript> {
        [Provider::Groq, Provider::Deepgram]
            .into_iter()
            .map(|provider| SourceTranscript {
                provider,
                text: text.to_owned(),
            })
            .collect()
    }

    #[test]
    fn spoken_new_line_and_new_paragraph_become_real_breaks() {
        let spoken = "first thought new line second thought";
        assert_eq!(
            format_locally(spoken.to_owned(), &both_providers_heard(spoken), &[]),
            "First thought.\nSecond thought"
        );
        let spoken = "hello new paragraph world";
        assert_eq!(
            format_locally(spoken.to_owned(), &both_providers_heard(spoken), &[]),
            "Hello.\n\nWorld."
        );
    }

    #[tokio::test]
    async fn a_chord_pressed_while_busy_is_refused_not_replayed() {
        let (tx, mut actions) = mpsc::unbounded_channel();
        let refused = Arc::new(Mutex::new(Vec::new()));
        let ended: EndedSink = {
            let refused = Arc::clone(&refused);
            Arc::new(move |id| refused.lock().unwrap().push(id))
        };
        for action in [
            Action::Start {
                id: 2,
                hotkey_at: Instant::now(),
            },
            Action::HandsFree(2),
            Action::Stop(2),
        ] {
            tx.send(action).unwrap();
        }
        // Busy for four polls: long enough to see every queued action.
        let mut polls = 0;
        let work = poll_fn(|cx| {
            polls += 1;
            if polls > 3 {
                Poll::Ready("transcript")
            } else {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        });
        assert_eq!(while_busy(work, &mut actions, &ended).await, "transcript");
        assert_eq!(*refused.lock().unwrap(), [2]);
        assert!(actions.try_recv().is_err(), "nothing left to replay");
    }
}
