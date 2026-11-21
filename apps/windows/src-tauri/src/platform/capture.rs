//! Microphone capture through cpal: WASAPI shared mode on Windows, the
//! default input device, converted to the 16 kHz mono s16le the providers
//! take.
//!
//! WASAPI never moves a stream to a new default device, so when the default
//! mic changes mid-Recording the stream is rebuilt from the new default and
//! the Recording carries on. Every cpal `Stream` lives on one capture thread
//! because it is not `Send` on every host.
//!
//! With Keep microphone ready on, a press attaches to the stream
//! [`ready`] keeps open instead, and opens its own only when that stream is
//! not ready.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{ErrorKind, FromSample, SampleFormat, SizedSample};
use tokio::sync::{Notify, oneshot};
use voisu_core::{
    ActiveCapture, AudioChunk, BoundaryError, BoundaryFuture, BoundaryKind, CaptureLimit,
    CapturedAudio, DeadlineClock,
};

use crate::audio::{self, CHUNK_BYTES, LEVEL_WINDOW, Resampler, TARGET_RATE};
use crate::settings::KeepMicReady;

mod device_cache;
mod ready;
use device_cache::InputCache;
pub(crate) use ready::ReadyMic;

/// The longest Recording, as on Linux.
pub const RECORDING_DEADLINE: Duration = Duration::from_secs(600);
/// Retained PCM never exceeds the Deadline's worth of audio.
const PCM_BYTE_CAP: usize = RECORDING_DEADLINE.as_secs() as usize * TARGET_RATE as usize * 2;
/// Shorter Recordings are rejected, as on Linux.
const MIN_RECORDING_BYTES: usize = CHUNK_BYTES;
/// A Recording whose peak never exceeds this is silent, as on Linux.
const SILENCE_FLOOR: u16 = 32;
const OPEN_TIMEOUT: Duration = Duration::from_secs(5);
/// WASAPI's play request can succeed without starting callbacks.
const CACHED_AUDIO_TIMEOUT: Duration = Duration::from_millis(350);
const STOP_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a Recording waits for a replacement after its mic disappears.
const REPLACEMENT_WAIT: Duration = Duration::from_secs(2);
const REPLACEMENT_POLL: Duration = Duration::from_millis(200);

#[cfg(not(target_os = "macos"))]
pub const MIC_BLOCKED: &str = "Windows is blocking the microphone. Open Settings → Privacy & security → Microphone and turn on access for desktop apps.";
#[cfg(not(target_os = "macos"))]
pub const NO_MIC: &str =
    "No microphone found. Connect one or pick a default input in Windows Sound settings.";
#[cfg(target_os = "macos")]
pub const MIC_BLOCKED: &str = "macOS is blocking the microphone. Open System Settings → Privacy & Security → Microphone and turn on Voisu.";
#[cfg(target_os = "macos")]
pub const NO_MIC: &str =
    "No microphone found. Connect one or pick an input in System Settings → Sound.";
const MIC_FAILED: &str = "The microphone stopped working.";

/// Receives one meter reading (0.0–1.0) about 30 times a second while a
/// Recording captures. Called from the audio thread: it must not block.
pub type LevelSink = Arc<dyn Fn(f32) + Send + Sync>;

#[derive(Default)]
pub(crate) struct CaptureStartup {
    stream: Mutex<StreamStartup>,
    /// Woken when this press opens the microphone itself (no ready stream).
    cold: Notify,
}

#[derive(Default)]
struct StreamStartup {
    thread_spawned: Option<Instant>,
    attempt: u64,
    device_resolved: Option<Instant>,
    stream_built: Option<Instant>,
    play_returned: Option<Instant>,
    first_audio: Option<Instant>,
}

impl CaptureStartup {
    /// Resolves once this press opens the microphone itself instead of
    /// taking a ready stream, so the overlay can show it is still starting.
    /// Never resolves once audio has arrived: the capture is then ready in
    /// all but name, and `Starting` would flash before `Recording`.
    pub(crate) async fn opening_cold(&self) {
        self.cold.notified().await;
        if self.stream.lock().unwrap().first_audio.is_some() {
            std::future::pending::<()>().await;
        }
    }

    fn begin_attempt(&self) -> (u64, bool) {
        let mut stream = self.stream.lock().unwrap();
        stream.attempt += 1;
        let measuring = stream.first_audio.is_none();
        if measuring {
            stream.device_resolved = None;
            stream.stream_built = None;
            stream.play_returned = None;
        }
        (stream.attempt, measuring)
    }

    fn note_first_audio(&self, attempt: u64, at: Instant) -> bool {
        let mut stream = self.stream.lock().unwrap();
        if stream.attempt != attempt {
            return false;
        }
        stream.first_audio.get_or_insert(at);
        true
    }

    fn keep_cached_attempt(&self) -> bool {
        let mut stream = self.stream.lock().unwrap();
        if stream.first_audio.is_some() {
            return true;
        }
        // Retire it before dropping it so a late callback can't count as
        // the replacement stream's first audio or enter its PCM queue.
        stream.attempt += 1;
        false
    }

    pub(crate) fn log(&self, id: u64, hotkey_at: Instant, providers_started: Option<Instant>) {
        let elapsed_ms =
            |at: Option<Instant>| at.map(|at| at.saturating_duration_since(hotkey_at).as_millis());
        let stream = self.stream.lock().unwrap();
        tracing::info!(
            id,
            thread_spawned_ms = ?elapsed_ms(stream.thread_spawned),
            device_resolved_ms = ?elapsed_ms(stream.device_resolved),
            stream_built_ms = ?elapsed_ms(stream.stream_built),
            play_returned_ms = ?elapsed_ms(stream.play_returned),
            first_audio_ms = ?elapsed_ms(stream.first_audio),
            providers_started_ms = ?elapsed_ms(providers_started),
            "Recording startup"
        );
    }
}

pub(crate) struct CpalCapture {
    input: InputCache,
    ready: ReadyMic,
}

impl CpalCapture {
    pub(crate) fn new(keep_ready: KeepMicReady) -> Self {
        let ready = ReadyMic::start(keep_ready);
        let watcher = ready.clone();
        Self {
            input: InputCache::new(move || watcher.default_changed()),
            ready,
        }
    }

    /// The handle the settings command uses to change Keep microphone ready.
    pub(crate) fn ready_mic(&self) -> ReadyMic {
        self.ready.clone()
    }

    /// Attaches to the ready mic, or opens the default mic off the runtime
    /// and waits for audio to flow.
    pub(crate) async fn begin(
        &self,
        recording_id: u64,
        levels: LevelSink,
        startup: Arc<CaptureStartup>,
    ) -> Result<Box<dyn ActiveCapture>, BoundaryError> {
        let shared = Arc::new(Shared {
            pipeline: Mutex::new(Pipeline::new(levels)),
            wake: Notify::new(),
            startup,
            stop: CaptureStop::default(),
        });
        let why = match self.ready.attach(&shared).await {
            Ok(preroll) => {
                let attached = Instant::now();
                shared.startup.stream.lock().unwrap().first_audio = Some(attached);
                tracing::info!(
                    id = recording_id,
                    start = "ready",
                    preroll_ms = preroll.as_millis(),
                    "microphone start"
                );
                return Ok(Box::new(CpalActiveCapture {
                    shared,
                    source: Source::Ready,
                    ready: Some(self.ready.clone()),
                    started: attached,
                    pcm: Vec::new(),
                }));
            }
            Err(why) => why,
        };
        tracing::info!(
            id = recording_id,
            start = "cold",
            preroll_ms = 0,
            why,
            "microphone start"
        );
        // A stored permit: the engine may not be waiting on it yet. Windows
        // only: there a cold open takes ~0.8 s, and the Mac never keeps a
        // ready stream, so it keeps showing nothing until the first audio.
        if cfg!(not(target_os = "macos")) {
            shared.startup.cold.notify_one();
        }
        let (control_tx, control_rx) = mpsc::channel();
        let (ready_tx, mut ready_rx) = oneshot::channel();
        let thread_shared = Arc::clone(&shared);
        let thread_control = control_tx.clone();
        let cached_input = self.input.take();
        let refresh = self.input.refresh_after_capture();
        let worker = thread::Builder::new()
            .name(format!("voisu-capture-{recording_id}"))
            .spawn(move || {
                thread_shared.startup.stream.lock().unwrap().thread_spawned = Some(Instant::now());
                run_capture(
                    thread_shared,
                    thread_control,
                    control_rx,
                    ready_tx,
                    cached_input,
                );
                refresh();
            })
            .map_err(|error| {
                BoundaryError::new(
                    BoundaryKind::Capture,
                    format!("capture thread unavailable: {error}"),
                )
            })?;
        // Owning the stop guard before awaiting also releases the stream if
        // startup is cancelled or times out.
        let mut capture = Box::new(CpalActiveCapture {
            shared,
            source: Source::Thread {
                control: control_tx,
                worker: Some(worker),
            },
            ready: Some(self.ready.clone()),
            started: Instant::now(),
            pcm: Vec::new(),
        });
        capture.started = tokio::time::timeout(OPEN_TIMEOUT, &mut ready_rx)
            .await
            .map_err(|_| mic_failed("microphone did not produce audio in time"))?
            .map_err(|_| mic_failed("capture thread stopped while opening the microphone"))?
            .map_err(Failure::into_error)?;
        Ok(capture)
    }
}

struct CpalActiveCapture {
    shared: Arc<Shared>,
    source: Source,
    /// Told once when this capture ends, so the ready mic can open or close.
    ready: Option<ReadyMic>,
    started: Instant,
    pcm: Vec<u8>,
}

/// Where the Recording's audio comes from.
enum Source {
    /// A stream this Recording opened on its own capture thread.
    Thread {
        control: mpsc::Sender<Control>,
        worker: Option<thread::JoinHandle<()>>,
    },
    /// The ready mic's stream, which outlives the Recording.
    Ready,
}

impl CpalActiveCapture {
    fn request_stop(&self) {
        self.shared.stop.request();
        match &self.source {
            Source::Thread { control, .. } => {
                let _ = control.send(Control::Stop);
            }
            Source::Ready => {
                if let Some(ready) = &self.ready {
                    ready.detach(&self.shared);
                }
            }
        }
    }

    /// Stops the stream, flushes the resampler tail into the queue, and waits
    /// (bounded) for the capture thread to exit.
    async fn stop(&mut self) {
        self.request_stop();
        match &mut self.source {
            Source::Thread { worker, .. } => {
                if let Some(worker) = worker.take() {
                    let joined = tokio::task::spawn_blocking(move || {
                        let deadline = Instant::now() + STOP_TIMEOUT;
                        while !worker.is_finished() && Instant::now() < deadline {
                            thread::sleep(Duration::from_millis(10));
                        }
                        if worker.is_finished() {
                            let _ = worker.join();
                        }
                    });
                    let _ = joined.await;
                }
            }
            // Detached above, so nothing else touches the pipeline.
            Source::Ready if self.ready.is_some() => finish_pipeline(&self.shared),
            Source::Ready => {}
        }
        self.release();
    }

    fn release(&mut self) {
        if let Some(ready) = self.ready.take() {
            ready.released();
        }
    }
}

impl ActiveCapture for CpalActiveCapture {
    fn deadline_clock(&self) -> DeadlineClock {
        DeadlineClock {
            started: self.started,
            deadline: RECORDING_DEADLINE,
        }
    }

    fn next_chunk(&mut self) -> BoundaryFuture<'_, Option<AudioChunk>> {
        Box::pin(async move {
            loop {
                if self.started.elapsed() >= RECORDING_DEADLINE {
                    return Err(BoundaryError::new(
                        BoundaryKind::RecordingDeadline,
                        "Recording Deadline elapsed",
                    ));
                }
                // Registered before the check so a chunk queued in between
                // still wakes this task.
                let woken = self.shared.wake.notified();
                let next = {
                    let mut pipeline = self.shared.pipeline.lock().unwrap();
                    if let Some(failure) = &pipeline.failure {
                        return Err(failure.clone().into_error());
                    }
                    (pipeline.chunks.pop_front(), pipeline.cap_reached)
                };
                match next {
                    (Some(chunk), _) => {
                        self.pcm.extend_from_slice(&chunk.0);
                        return Ok(Some(chunk));
                    }
                    (None, true) => return Ok(None),
                    (None, false) => {
                        let remaining = RECORDING_DEADLINE.saturating_sub(self.started.elapsed());
                        let _ = tokio::time::timeout(remaining, woken).await;
                    }
                }
            }
        })
    }

    fn finish(&mut self) -> BoundaryFuture<'_, CapturedAudio> {
        Box::pin(async move {
            self.stop().await;
            let cap_reached = {
                let mut pipeline = self.shared.pipeline.lock().unwrap();
                if let Some(failure) = &pipeline.failure {
                    return Err(failure.clone().into_error());
                }
                pipeline.flush_partial();
                for chunk in pipeline.chunks.drain(..) {
                    self.pcm.extend_from_slice(&chunk.0);
                }
                pipeline.cap_reached
            };
            validate(&self.pcm)?;
            let pcm = std::mem::take(&mut self.pcm);
            Ok(if cap_reached {
                CapturedAudio::truncated(pcm, CaptureLimit::Buffer)
            } else {
                CapturedAudio::new(pcm)
            })
        })
    }

    fn abort(mut self: Box<Self>) -> BoundaryFuture<'static, ()> {
        Box::pin(async move {
            self.stop().await;
            Ok(())
        })
    }
}

impl Drop for CpalActiveCapture {
    fn drop(&mut self) {
        // A dropped capture (panic, cancelled task) still releases the mic.
        self.request_stop();
        self.release();
    }
}

#[derive(Default)]
struct CaptureStop {
    requested: AtomicBool,
    play: Mutex<()>,
}

impl CaptureStop {
    fn requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    fn request(&self) {
        self.requested.store(true, Ordering::Release);
        // After this returns even a worker currently inside play cannot
        // start another stream. Device lookup and build never hold this lock.
        drop(self.play.lock().unwrap());
    }

    fn play_if_running<T>(&self, play: impl FnOnce() -> T) -> Option<T> {
        let _play = self.play.lock().unwrap();
        (!self.requested()).then(play)
    }
}

fn validate(pcm: &[u8]) -> Result<(), BoundaryError> {
    if pcm.is_empty() {
        return Err(BoundaryError::new(
            BoundaryKind::EmptyRecording,
            "microphone returned no audio",
        ));
    }
    if pcm.len() < MIN_RECORDING_BYTES {
        return Err(BoundaryError::new(
            BoundaryKind::TooShortRecording,
            format!("Recording contained {} PCM bytes", pcm.len()),
        ));
    }
    let audible = pcm
        .as_chunks::<2>()
        .0
        .iter()
        .any(|&sample| i16::from_le_bytes(sample).unsigned_abs() > SILENCE_FLOOR);
    if !audible {
        return Err(BoundaryError::new(
            BoundaryKind::SilentRecording,
            "Recording peak amplitude did not exceed the silence floor",
        ));
    }
    Ok(())
}

enum Control {
    Stop,
    Stream(u64, StreamEvent),
}

enum StreamEvent {
    Audio,
    /// The stream's device went away or its default changed.
    Reopen,
    Fatal(Failure),
}

/// A capture failure that can be stored and replayed to every caller.
#[derive(Clone, Debug)]
struct Failure {
    kind: BoundaryKind,
    diagnostic: String,
    public: &'static str,
}

impl Failure {
    fn new(kind: BoundaryKind, diagnostic: String, public: &'static str) -> Self {
        Self {
            kind,
            diagnostic,
            public,
        }
    }

    fn into_error(self) -> BoundaryError {
        BoundaryError::new(self.kind, self.diagnostic).with_public_message(self.public)
    }
}

fn mic_failed(diagnostic: &str) -> BoundaryError {
    BoundaryError::new(BoundaryKind::Capture, diagnostic).with_public_message(MIC_FAILED)
}

fn capture_stopped() -> Failure {
    Failure::new(
        BoundaryKind::Capture,
        "capture already stopped".into(),
        MIC_FAILED,
    )
}

struct Shared {
    pipeline: Mutex<Pipeline>,
    wake: Notify,
    startup: Arc<CaptureStartup>,
    stop: CaptureStop,
}

/// Device samples in, provider chunks and meter readings out. Survives a
/// stream rebuild so no partial chunk is lost when the mic changes.
struct Pipeline {
    resampler: Resampler,
    channels: usize,
    mono: Vec<f32>,
    resampled: Vec<f32>,
    /// Converted samples not yet a full chunk.
    partial: Vec<u8>,
    meter: Vec<f32>,
    levels: LevelSink,
    chunks: VecDeque<AudioChunk>,
    produced: usize,
    cap_reached: bool,
    failure: Option<Failure>,
}

impl Pipeline {
    fn new(levels: LevelSink) -> Self {
        Self {
            resampler: Resampler::new(TARGET_RATE),
            channels: 1,
            mono: Vec::new(),
            resampled: Vec::new(),
            partial: Vec::with_capacity(CHUNK_BYTES),
            meter: Vec::with_capacity(LEVEL_WINDOW),
            levels,
            chunks: VecDeque::new(),
            produced: 0,
            cap_reached: false,
            failure: None,
        }
    }

    /// Switches to a new device format, first draining what the old
    /// resampler still holds.
    fn reformat(&mut self, rate: u32, channels: usize) {
        self.resampled.clear();
        self.resampler.flush(&mut self.resampled);
        self.emit_resampled();
        self.resampler = Resampler::new(rate);
        self.channels = channels.max(1);
    }

    fn push(&mut self, interleaved: &[f32]) {
        if self.cap_reached {
            return;
        }
        self.mono.clear();
        audio::downmix(interleaved, self.channels, &mut self.mono);
        self.resampled.clear();
        let Self {
            resampler,
            mono,
            resampled,
            ..
        } = self;
        resampler.process(mono, resampled);
        self.emit_resampled();
    }

    fn emit_resampled(&mut self) {
        for index in 0..self.resampled.len() {
            let sample = self.resampled[index];
            self.meter.push(sample);
            if self.meter.len() == LEVEL_WINDOW {
                (self.levels)(audio::level(&self.meter));
                self.meter.clear();
            }
        }
        let resampled = std::mem::take(&mut self.resampled);
        for block in resampled.chunks(CHUNK_BYTES / 2) {
            audio::push_s16le(block, &mut self.partial);
            if self.partial.len() >= CHUNK_BYTES {
                let rest = self.partial.split_off(CHUNK_BYTES);
                let chunk = std::mem::replace(&mut self.partial, rest);
                self.queue(chunk);
            }
        }
        self.resampled = resampled;
    }

    fn flush_partial(&mut self) {
        if !self.partial.is_empty() {
            let chunk = std::mem::take(&mut self.partial);
            self.queue(chunk);
        }
    }

    fn queue(&mut self, mut chunk: Vec<u8>) {
        if self.cap_reached {
            return;
        }
        let room = PCM_BYTE_CAP - self.produced;
        if chunk.len() >= room {
            chunk.truncate(room);
            self.cap_reached = true;
        }
        self.produced += chunk.len();
        if !chunk.is_empty() {
            self.chunks.push_back(AudioChunk(chunk));
        }
    }
}

/// The capture thread: owns the stream, rebuilds it when the default mic
/// changes, and drains the resampler when told to stop.
fn run_capture(
    shared: Arc<Shared>,
    control_tx: mpsc::Sender<Control>,
    control_rx: mpsc::Receiver<Control>,
    ready: oneshot::Sender<Result<Instant, Failure>>,
    cached_input: Option<ResolvedInput>,
) {
    if microphone_blocked() {
        let _ = ready.send(Err(Failure::new(
            BoundaryKind::Capture,
            "microphone access is denied in privacy settings".into(),
            MIC_BLOCKED,
        )));
        return;
    }
    if ready.is_closed() {
        return;
    }
    let host = cpal::default_host();
    let mut pending = None;
    let opened = with_fresh_input(
        cached_input,
        |input, cached| {
            // CoreAudio applies the supplied format to the device when
            // building. A format change need not change the endpoint ID.
            #[cfg(target_os = "macos")]
            let input = if cached {
                let config = input_config(&input.device)?;
                ResolvedInput { config, ..input }
            } else {
                input
            };
            let stream = open_stream(input, &host, &shared, &control_tx)?;
            if cached {
                let event = control_rx.recv_timeout(CACHED_AUDIO_TIMEOUT).ok();
                if shared.stop.requested() {
                    return Err(capture_stopped());
                }
                if !shared.startup.keep_cached_attempt() {
                    return Err(Failure::new(
                        BoundaryKind::Capture,
                        "cached microphone did not produce audio".into(),
                        MIC_FAILED,
                    ));
                }
                pending = event;
            }
            Ok(stream)
        },
        || {
            if shared.stop.requested() {
                return Err(capture_stopped());
            }
            ResolvedInput::resolve(&host)
        },
    );
    let mut stream = match opened {
        Ok(stream) => stream,
        Err(failure) => {
            let _ = ready.send(Err(failure));
            return;
        }
    };
    let mut ready = Some(ready);
    loop {
        // The worker observes readiness only once play has returned, even
        // if the stream produced audio during play.
        if ready.is_some() {
            let first_audio = shared.startup.stream.lock().unwrap().first_audio;
            if let Some(first_audio) = first_audio
                && let Some(ready) = ready.take()
                && ready.send(Ok(first_audio)).is_err()
            {
                break;
            }
        }
        let event = pending.take().or_else(|| control_rx.recv().ok());
        match event {
            Some(Control::Stop) | None => break,
            Some(Control::Stream(attempt, _))
                if shared.startup.stream.lock().unwrap().attempt != attempt => {}
            Some(Control::Stream(_, StreamEvent::Audio)) => {}
            Some(Control::Stream(_, StreamEvent::Fatal(failure))) => {
                drop(stream);
                return fail_capture(&shared, ready, failure);
            }
            Some(Control::Stream(_, StreamEvent::Reopen)) => {
                drop(stream);
                match reopen(&host, &shared, &control_tx, &control_rx) {
                    Reopened::Stream(next) => stream = next,
                    Reopened::Stopped => return finish_pipeline(&shared),
                    Reopened::Failed(failure) => return fail_capture(&shared, ready, failure),
                }
            }
        }
    }
    drop(stream);
    finish_pipeline(&shared);
}

enum Reopened {
    Stream(cpal::Stream),
    Stopped,
    Failed(Failure),
}

/// Rebuilds from the current default mic, waiting briefly for a replacement
/// when none exists yet (a headset reconnecting).
fn reopen(
    host: &cpal::Host,
    shared: &Arc<Shared>,
    control_tx: &mpsc::Sender<Control>,
    control_rx: &mpsc::Receiver<Control>,
) -> Reopened {
    let deadline = Instant::now() + REPLACEMENT_WAIT;
    loop {
        if shared.stop.requested() {
            return Reopened::Stopped;
        }
        let opened = ResolvedInput::resolve(host)
            .and_then(|input| open_stream(input, host, shared, control_tx));
        if shared.stop.requested() {
            return Reopened::Stopped;
        }
        match opened {
            Ok(stream) => {
                tracing::info!("capture moved to the new default microphone");
                return Reopened::Stream(stream);
            }
            Err(failure) if failure.public != NO_MIC || Instant::now() >= deadline => {
                return Reopened::Failed(failure);
            }
            Err(_) => {}
        }
        match control_rx.recv_timeout(REPLACEMENT_POLL) {
            Ok(Control::Stop) | Err(RecvTimeoutError::Disconnected) => return Reopened::Stopped,
            // A stale signal from the stream just dropped.
            Ok(Control::Stream(_, _)) | Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

fn finish_pipeline(shared: &Shared) {
    let mut pipeline = shared.pipeline.lock().unwrap();
    pipeline.resampled.clear();
    let Pipeline {
        resampler,
        resampled,
        ..
    } = &mut *pipeline;
    resampler.flush(resampled);
    pipeline.emit_resampled();
    drop(pipeline);
    shared.wake.notify_one();
}

fn fail_capture(
    shared: &Shared,
    ready: Option<oneshot::Sender<Result<Instant, Failure>>>,
    failure: Failure,
) {
    tracing::warn!(diagnostic = %failure.diagnostic, "capture failed");
    if let Some(ready) = ready {
        let _ = ready.send(Err(failure));
    } else {
        shared.pipeline.lock().unwrap().failure = Some(failure);
        shared.wake.notify_one();
    }
}

struct ResolvedInput {
    device: cpal::Device,
    id: cpal::DeviceId,
    config: cpal::SupportedStreamConfig,
}

impl ResolvedInput {
    fn resolve(host: &cpal::Host) -> Result<Self, Failure> {
        let device = host.default_input_device().ok_or_else(|| {
            Failure::new(
                BoundaryKind::Capture,
                "no default input device".into(),
                NO_MIC,
            )
        })?;
        let id = device
            .id()
            .map_err(|error| classify("default input ID unavailable", &error))?;
        let config = input_config(&device)?;
        Ok(Self { device, id, config })
    }

    /// The current default mic's ID, if there is one.
    fn default_id(host: &cpal::Host) -> Option<cpal::DeviceId> {
        host.default_input_device()
            .and_then(|device| device.id().ok())
    }

    fn verify_default(&self, host: &cpal::Host) -> Result<(), Failure> {
        if Self::default_id(host).as_ref() == Some(&self.id) {
            return Ok(());
        }
        Err(Failure::new(
            BoundaryKind::Capture,
            "default microphone changed before start".into(),
            NO_MIC,
        ))
    }
}

fn input_config(device: &cpal::Device) -> Result<cpal::SupportedStreamConfig, Failure> {
    device
        .default_input_config()
        .map_err(|error| classify("default input config unavailable", &error))
}

fn with_fresh_input<I, T, E>(
    cached: Option<I>,
    mut open: impl FnMut(I, bool) -> Result<T, E>,
    resolve: impl FnOnce() -> Result<I, E>,
) -> Result<T, E> {
    if let Some(input) = cached
        && let Ok(stream) = open(input, true)
    {
        return Ok(stream);
    }
    open(resolve()?, false)
}

fn open_stream(
    input: ResolvedInput,
    host: &cpal::Host,
    shared: &Arc<Shared>,
    control_tx: &mpsc::Sender<Control>,
) -> Result<cpal::Stream, Failure> {
    if shared.stop.requested() {
        return Err(capture_stopped());
    }
    let startup = &shared.startup;
    let (attempt, measuring) = startup.begin_attempt();
    input.verify_default(host)?;
    if measuring {
        startup.stream.lock().unwrap().device_resolved = Some(Instant::now());
    }
    let device = &input.device;
    let supported = &input.config;
    let format = supported.sample_format();
    let config = cpal::StreamConfig {
        channels: supported.channels(),
        sample_rate: supported.sample_rate(),
        buffer_size: cpal::BufferSize::Default,
    };
    tracing::info!(
        rate = config.sample_rate,
        channels = config.channels,
        ?format,
        "opening microphone"
    );
    shared
        .pipeline
        .lock()
        .unwrap()
        .reformat(config.sample_rate, usize::from(config.channels));
    let stream = build_stream(
        device,
        config,
        format,
        recording_delivery(shared, control_tx, attempt),
        {
            let control = control_tx.clone();
            move |event| {
                let _ = control.send(Control::Stream(attempt, event));
            }
        },
    )?;
    if measuring {
        startup.stream.lock().unwrap().stream_built = Some(Instant::now());
    }
    // CPAL has installed its stream listeners by now. Verify again in case
    // the endpoint changed while it was building the stream.
    if shared.stop.requested() {
        return Err(capture_stopped());
    }
    input.verify_default(host)?;
    let played = shared
        .stop
        .play_if_running(|| stream.play())
        .ok_or_else(capture_stopped)?;
    if measuring {
        startup.stream.lock().unwrap().play_returned = Some(Instant::now());
    }
    played.map_err(|error| classify("input stream did not start", &error))?;
    Ok(stream)
}

/// Feeds one Recording's own stream into its pipeline.
fn recording_delivery(
    shared: &Arc<Shared>,
    control_tx: &mpsc::Sender<Control>,
    attempt: u64,
) -> impl FnMut(&[f32]) + Send + 'static {
    let shared = Arc::clone(shared);
    let audio = control_tx.clone();
    let mut accepts_audio = None;
    move |samples| {
        if shared.stop.requested() {
            return;
        }
        let first_callback = accepts_audio.is_none();
        if !*accepts_audio
            .get_or_insert_with(|| shared.startup.note_first_audio(attempt, Instant::now()))
        {
            return;
        }
        let mut pipeline = shared.pipeline.lock().unwrap();
        if shared.stop.requested() {
            return;
        }
        pipeline.push(samples);
        drop(pipeline);
        shared.wake.notify_one();
        if first_callback {
            let _ = audio.send(Control::Stream(attempt, StreamEvent::Audio));
        }
    }
}

/// Builds an input stream that hands `deliver` each buffer as interleaved
/// f32 and `report` the first error that needs action.
fn build_stream(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    format: SampleFormat,
    deliver: impl FnMut(&[f32]) + Send + 'static,
    report: impl FnMut(StreamEvent) + Send + 'static,
) -> Result<cpal::Stream, Failure> {
    match format {
        SampleFormat::F32 => build::<f32>(device, config, deliver, report),
        SampleFormat::F64 => build::<f64>(device, config, deliver, report),
        SampleFormat::I16 => build::<i16>(device, config, deliver, report),
        SampleFormat::I32 => build::<i32>(device, config, deliver, report),
        SampleFormat::U16 => build::<u16>(device, config, deliver, report),
        SampleFormat::U8 => build::<u8>(device, config, deliver, report),
        SampleFormat::I8 => build::<i8>(device, config, deliver, report),
        other => {
            return Err(Failure::new(
                BoundaryKind::Capture,
                format!("unsupported sample format {other:?}"),
                MIC_FAILED,
            ));
        }
    }
    .map_err(|error| classify("input stream unavailable", &error))
}

fn build<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut deliver: impl FnMut(&[f32]) + Send + 'static,
    mut report: impl FnMut(StreamEvent) + Send + 'static,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    // Reused for every callback: the audio thread never allocates once the
    // first buffer has sized it.
    let mut converted: Vec<f32> = Vec::new();
    // Once the stream has failed, later callbacks are ignored.
    let mut reported = false;
    device.build_input_stream::<T, _, _>(
        config,
        move |data: &[T], _| {
            if data.is_empty() {
                return;
            }
            converted.clear();
            converted.extend(
                data.iter()
                    .map(|&sample| <f32 as FromSample<T>>::from_sample_(sample)),
            );
            deliver(&converted);
        },
        move |error: cpal::Error| {
            if reported {
                return;
            }
            let event = match error.kind() {
                // Glitches and refused real-time priority: keep recording.
                // `DeviceChanged` means cpal already rerouted the stream; a
                // WASAPI default-mic switch arrives as `StreamInvalidated`.
                ErrorKind::Xrun | ErrorKind::RealtimeDenied | ErrorKind::DeviceChanged => return,
                ErrorKind::StreamInvalidated | ErrorKind::DeviceNotAvailable => StreamEvent::Reopen,
                _ => StreamEvent::Fatal(classify("input stream failed", &error)),
            };
            reported = true;
            report(event);
        },
        None,
    )
}

/// Maps a cpal failure to what the user can act on.
fn classify(context: &str, error: &cpal::Error) -> Failure {
    let diagnostic = format!("{context}: {error}");
    let public = match error.kind() {
        ErrorKind::PermissionDenied => MIC_BLOCKED,
        ErrorKind::DeviceNotAvailable => NO_MIC,
        _ if access_denied(&diagnostic) => MIC_BLOCKED,
        _ => MIC_FAILED,
    };
    Failure::new(BoundaryKind::Capture, diagnostic, public)
}

/// WASAPI reports a privacy block as a generic backend error carrying
/// E_ACCESSDENIED.
fn access_denied(diagnostic: &str) -> bool {
    let lower = diagnostic.to_ascii_lowercase();
    lower.contains("0x80070005")
        || lower.contains("-2147024891")
        || lower.contains("access is denied")
}

/// Whether Windows privacy settings deny this desktop app the mic: the
/// global switch (machine or user) or the desktop-apps switch.
#[cfg(windows)]
fn microphone_blocked() -> bool {
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW,
    };
    use windows::core::w;

    fn denied(root: HKEY, key: windows::core::PCWSTR) -> bool {
        let mut buffer = [0u16; 16];
        let mut size = std::mem::size_of_val(&buffer) as u32;
        // SAFETY: the buffer and its byte size describe valid writable memory.
        let status = unsafe {
            RegGetValueW(
                root,
                key,
                w!("Value"),
                RRF_RT_REG_SZ,
                None,
                Some(buffer.as_mut_ptr().cast()),
                Some(&mut size),
            )
        };
        if status.is_err() {
            return false;
        }
        let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
        String::from_utf16_lossy(&buffer[..len]).eq_ignore_ascii_case("Deny")
    }

    let store = w!(
        "Software\\Microsoft\\Windows\\CurrentVersion\\CapabilityAccessManager\\ConsentStore\\microphone"
    );
    let desktop = w!(
        "Software\\Microsoft\\Windows\\CurrentVersion\\CapabilityAccessManager\\ConsentStore\\microphone\\NonPackaged"
    );
    denied(HKEY_LOCAL_MACHINE, store)
        || denied(HKEY_CURRENT_USER, store)
        || denied(HKEY_CURRENT_USER, desktop)
}

/// Denied or restricted in Privacy & Security. Not yet asked is not
/// blocked: opening the mic shows the system prompt.
#[cfg(target_os = "macos")]
fn microphone_blocked() -> bool {
    crate::platform::permissions::microphone_denied()
}

#[cfg(not(any(windows, target_os = "macos")))]
fn microphone_blocked() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn opens_the_current_default_when_the_cached_input_fails() {
        let opened = with_fresh_input(
            Some("disconnected mic"),
            |input, _| match input {
                "connected mic" => Ok(input),
                _ => Err("device unavailable"),
            },
            || Ok("connected mic"),
        );
        assert_eq!(opened, Ok("connected mic"));
    }

    #[test]
    fn opens_the_warm_input_without_resolving_the_default_again() {
        let opened = with_fresh_input(
            Some("warm mic"),
            |input, _| Ok::<_, &str>(input),
            || panic!("the cached device opened"),
        );
        assert_eq!(opened, Ok("warm mic"));
    }

    #[test]
    fn dropped_capture_refuses_to_play_a_stream_built_later() {
        let (capture, commands) = capture_with_pipeline(pipeline().0);
        let shared = capture.shared.clone();
        assert_eq!(shared.stop.play_if_running(|| "live"), Some("live"));
        drop(capture);
        assert!(matches!(commands.try_recv(), Ok(Control::Stop)));
        assert_eq!(
            shared.stop.play_if_running(|| panic!("late play")),
            None::<()>
        );
    }

    #[test]
    fn a_silent_cached_attempt_is_retried_and_late_callbacks_are_ignored() {
        let startup = CaptureStartup::default();
        let (cached, measuring) = startup.begin_attempt();
        assert!(measuring);
        startup.stream.lock().unwrap().play_returned = Some(Instant::now());
        let stream = with_fresh_input(
            Some("cached mic"),
            |input, cached| {
                if cached && !startup.keep_cached_attempt() {
                    return Err("no callback");
                }
                Ok(input)
            },
            || Ok("fresh mic"),
        );
        assert_eq!(stream, Ok("fresh mic"));
        let (replacement, measuring) = startup.begin_attempt();
        assert!(measuring);
        assert!(!startup.note_first_audio(cached, Instant::now()));
        assert!(startup.stream.lock().unwrap().play_returned.is_none());
        let first = Instant::now();
        assert!(startup.note_first_audio(replacement, first));
        assert_eq!(startup.stream.lock().unwrap().first_audio, Some(first));
    }

    #[tokio::test]
    async fn a_cold_open_shows_starting_only_until_its_first_audio() {
        let startup = CaptureStartup::default();
        startup.cold.notify_one();
        tokio::time::timeout(Duration::from_secs(1), startup.opening_cold())
            .await
            .expect("still opening: Starting shows");

        // The first callback beats the worker's readiness: no Starting.
        let startup = CaptureStartup::default();
        let (attempt, _) = startup.begin_attempt();
        startup.cold.notify_one();
        assert!(startup.note_first_audio(attempt, Instant::now()));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), startup.opening_cold())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn buffered_startup_pcm_is_streamed_once_and_retained_for_completion() {
        let (mut pipeline, _) = pipeline();
        let startup = vec![1; CHUNK_BYTES];
        let next = vec![2; CHUNK_BYTES];
        pipeline.queue(startup.clone());
        pipeline.queue(next.clone());
        pipeline.partial.extend([3, 3]);
        let (mut capture, _commands) = capture_with_pipeline(pipeline);

        assert_eq!(capture.next_chunk().await.unwrap().unwrap().0, startup);
        assert_eq!(capture.next_chunk().await.unwrap().unwrap().0, next);
        let audio = capture.finish().await.unwrap();
        let expected = [startup, next, vec![3, 3]].concat();
        assert_eq!(audio.pcm_s16le_mono_16khz(), expected);
    }

    pub(super) fn capture_with_pipeline(
        pipeline: Pipeline,
    ) -> (CpalActiveCapture, mpsc::Receiver<Control>) {
        let (control, commands) = mpsc::channel();
        let capture = CpalActiveCapture {
            shared: Arc::new(Shared {
                pipeline: Mutex::new(pipeline),
                wake: Notify::new(),
                startup: Arc::default(),
                stop: CaptureStop::default(),
            }),
            source: Source::Thread {
                control,
                worker: None,
            },
            ready: None,
            started: Instant::now(),
            pcm: Vec::new(),
        };
        (capture, commands)
    }

    pub(super) fn pipeline() -> (Pipeline, Arc<AtomicUsize>) {
        let readings = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&readings);
        let sink: LevelSink = Arc::new(move |level| {
            assert!((0.0..=1.0).contains(&level));
            counter.fetch_add(1, Ordering::Relaxed);
        });
        (Pipeline::new(sink), readings)
    }

    fn queued_bytes(pipeline: &Pipeline) -> usize {
        pipeline.chunks.iter().map(|c| c.0.len()).sum()
    }

    #[test]
    fn pipeline_emits_full_chunks_and_meters_at_thirty_hertz() {
        let (mut p, readings) = pipeline();
        p.reformat(48_000, 2);
        // One second of stereo 48 kHz in 10 ms device buffers.
        let buffer = vec![0.25f32; 960];
        for _ in 0..100 {
            p.push(&buffer);
        }
        assert!(p.chunks.iter().all(|c| c.0.len() == CHUNK_BYTES));
        let mut total = queued_bytes(&p) + p.partial.len();
        assert!(total.abs_diff(32_000) <= 2 * 40, "{total}");
        p.flush_partial();
        total = queued_bytes(&p);
        assert_eq!(p.partial.len(), 0);
        assert!(total > 31_000);
        let n = readings.load(Ordering::Relaxed);
        assert!((28..=30).contains(&n), "{n} readings");
    }

    #[test]
    fn reformat_keeps_the_partial_chunk() {
        let (mut p, _) = pipeline();
        p.reformat(48_000, 1);
        p.push(&vec![0.1; 480]);
        p.reformat(44_100, 1);
        p.push(&vec![0.1; 441]);
        p.flush_partial();
        // ~20 ms of 16 kHz audio survives the switch.
        let bytes = queued_bytes(&p);
        assert!(bytes.abs_diff(640) <= 80, "{bytes}");
    }

    #[test]
    fn pipeline_truncates_at_the_cap() {
        let (mut p, _) = pipeline();
        p.produced = PCM_BYTE_CAP - 100;
        p.queue(vec![1; CHUNK_BYTES]);
        assert!(p.cap_reached);
        assert_eq!(p.produced, PCM_BYTE_CAP);
        assert_eq!(queued_bytes(&p), 100);
        p.queue(vec![1; CHUNK_BYTES]);
        assert_eq!(p.produced, PCM_BYTE_CAP);
    }

    #[test]
    fn validation_matches_linux() {
        assert_eq!(
            validate(&[]).unwrap_err().kind(),
            BoundaryKind::EmptyRecording
        );
        assert_eq!(
            validate(&[0; 100]).unwrap_err().kind(),
            BoundaryKind::TooShortRecording
        );
        assert_eq!(
            validate(&[0; CHUNK_BYTES]).unwrap_err().kind(),
            BoundaryKind::SilentRecording
        );
        let mut loud = vec![0; CHUNK_BYTES];
        loud[10..12].copy_from_slice(&1000i16.to_le_bytes());
        assert!(validate(&loud).is_ok());
    }

    #[test]
    fn access_denied_is_detected_in_backend_messages() {
        assert!(access_denied(
            "input stream unavailable: Access is denied. (0x80070005)"
        ));
        assert!(access_denied("os error -2147024891"));
        assert!(!access_denied("The device is busy"));
    }
}
