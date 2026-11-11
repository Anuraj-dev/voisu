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
    ActiveCapture, AudioCapture, BoundaryError, BoundaryFuture, BoundaryKind, CancelRegistry,
    CaptureLimit, Credential, DEFAULT_RENDERING_POLICY, EnglishEligibilityOutcome,
    PreparedTranscriptDecision, Provider, ProviderCoordinator, ProviderStreams,
    ResolvedRecordingLanguages, SecretStore, SourceTranscript, Tone, TranscriptProvider,
    TranscriptValidator, apply_tone, sanitize_source_transcripts,
};

use crate::gesture::Action;
use crate::overlay::{
    self, AUDIO_LEVEL, AudioLevelPayload, RECORDING_STATE, RecordingState as Phase,
    RecordingStatePayload,
};
use crate::platform::capture::{CpalCapture, LevelSink};
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

/// What the capture phase leaves for the processing phase.
enum Captured {
    /// Nothing left to do; the state is already reported.
    Done,
    Process {
        capture: Box<dyn ActiveCapture>,
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
                Action::Start(id) => {
                    let captured = self.record(id, &mut actions).await;
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
    async fn record(&self, id: u64, actions: &mut UnboundedReceiver<Action>) -> Captured {
        // First, before any Voisu window (settings on missing keys) can take
        // focus: the Transcript goes where the user was typing.
        let target = DeliveryTarget::capture();
        // A tray preview still playing must not hide or feed this session's
        // capsule.
        overlay::end_preview();
        if !crate::keys_ready() {
            tracing::info!(id, "Recording refused: keys missing");
            self.emit_state(Phase::Error, Some(KEYS_MISSING.to_owned()));
            crate::show_settings(&self.app);
            return Captured::Done;
        }
        self.emit_state(Phase::Recording, None);

        let levels = self.level_sink();
        let begun = tokio::task::spawn_blocking(move || CpalCapture::new(levels).begin(id)).await;
        let mut capture = match begun {
            Ok(Ok(capture)) => capture,
            Ok(Err(error)) => {
                self.fail(id, &error);
                return Captured::Done;
            }
            Err(join) => {
                let error = BoundaryError::new(
                    BoundaryKind::Capture,
                    format!("capture start failed: {join}"),
                );
                self.fail(id, &error);
                return Captured::Done;
            }
        };
        let settings = self.settings();
        let vocabulary = vocabulary::normalize(&settings.boost_words);
        let tone = settings.tone;
        let streams = match self.start_providers(id, &vocabulary) {
            Ok(streams) => streams,
            Err(error) => {
                let _ = capture.abort().await;
                self.fail(id, &error);
                return Captured::Done;
            }
        };
        let mut providers = ProviderCoordinator::start(PROVIDER_DEADLINE, ABORT_DEADLINE, streams);
        match self.pump(id, &mut capture, &mut providers, actions).await {
            Outcome::Stop(limit) => Captured::Process {
                capture,
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
        let (mut capture, providers, limit, target, vocabulary, tone) = match captured {
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
                capture,
                providers,
                limit,
                target,
                vocabulary,
                tone,
            } => (capture, providers, limit, target, vocabulary, tone),
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
                self.deliver(id, target, text).await;
            }
            Err(error) => self.fail(id, &error),
        }
    }

    /// Pastes the Transcript into `target`, or with auto-paste off only puts
    /// it on the clipboard. Delivery blocks and pumps its own messages, so it
    /// runs on a blocking thread, which may keep the user's clipboard parked
    /// a little after the result arrives here.
    async fn deliver(&self, id: u64, target: DeliveryTarget, text: String) {
        if !self.settings().auto_paste {
            let copied = tokio::task::spawn_blocking(move || delivery::copy(&text)).await;
            let message = match copied {
                Ok(Ok(())) => {
                    return self.emit_state(Phase::Done, Some(delivery::COPIED.to_owned()));
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
            }
            Err(_) => {
                tracing::warn!(id, "delivery task ended without a result");
                self.delivered(Some("Couldn't paste the Transcript".to_owned()));
            }
        }
    }

    /// Success unless there is something to tell the user.
    fn delivered(&self, message: Option<String>) {
        match message {
            None => self.emit_state(Phase::Done, None),
            Some(message) => self.emit_state(Phase::Error, Some(message)),
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

    /// Starts both providers biased toward `vocabulary`, within the same
    /// keyterm and prompt budgets as Linux.
    fn start_providers(
        &self,
        id: u64,
        vocabulary: &[String],
    ) -> Result<ProviderStreams, BoundaryError> {
        let mut deepgram = DeepgramProvider::with_keyterms_and_language(
            self.host.clone(),
            vocabulary::deepgram_keyterms(vocabulary),
            LANGUAGE.to_owned(),
        );
        let mut groq = GroqProvider::with_prompt_and_language(
            self.host.clone(),
            vocabulary::whisper_prompt(vocabulary),
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
                if let Action::Start(id) = action {
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
        for action in [Action::Start(2), Action::HandsFree(2), Action::Stop(2)] {
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
