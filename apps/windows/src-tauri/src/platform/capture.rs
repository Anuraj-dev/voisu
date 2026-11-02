//! Microphone capture through cpal: WASAPI shared mode on Windows, the
//! default input device, converted to the 16 kHz mono s16le the providers
//! take.
//!
//! WASAPI never moves a stream to a new default device, so when the default
//! mic changes mid-Recording the stream is rebuilt from the new default and
//! the Recording carries on. Every cpal `Stream` lives on one capture thread
//! because it is not `Send` on every host.

use std::collections::VecDeque;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{ErrorKind, FromSample, SampleFormat, SizedSample};
use tokio::sync::Notify;
use voisu_core::{
    ActiveCapture, AudioCapture, AudioChunk, BoundaryError, BoundaryFuture, BoundaryKind,
    CaptureLimit, CapturedAudio, DeadlineClock,
};

use crate::audio::{self, CHUNK_BYTES, LEVEL_WINDOW, Resampler, TARGET_RATE};

/// The longest Recording, as on Linux.
pub const RECORDING_DEADLINE: Duration = Duration::from_secs(600);
/// Retained PCM never exceeds the Deadline's worth of audio.
const PCM_BYTE_CAP: usize = RECORDING_DEADLINE.as_secs() as usize * TARGET_RATE as usize * 2;
/// Shorter Recordings are rejected, as on Linux.
const MIN_RECORDING_BYTES: usize = CHUNK_BYTES;
/// A Recording whose peak never exceeds this is silent, as on Linux.
const SILENCE_FLOOR: u16 = 32;
const OPEN_TIMEOUT: Duration = Duration::from_secs(5);
const STOP_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a Recording waits for a replacement after its mic disappears.
const REPLACEMENT_WAIT: Duration = Duration::from_secs(2);
const REPLACEMENT_POLL: Duration = Duration::from_millis(200);

pub const MIC_BLOCKED: &str = "Windows is blocking the microphone. Open Settings → Privacy & security → Microphone and turn on access for desktop apps.";
pub const NO_MIC: &str =
    "No microphone found. Connect one or pick a default input in Windows Sound settings.";
const MIC_FAILED: &str = "The microphone stopped working.";

/// Receives one meter reading (0.0–1.0) about 30 times a second while a
/// Recording captures. Called from the audio thread: it must not block.
pub type LevelSink = Arc<dyn Fn(f32) + Send + Sync>;

pub struct CpalCapture {
    levels: LevelSink,
}

impl CpalCapture {
    pub fn new(levels: LevelSink) -> Self {
        Self { levels }
    }
}

impl AudioCapture for CpalCapture {
    /// Opens the default mic and starts capturing. Blocks until the stream
    /// runs or fails, so call it off the async runtime.
    fn begin(&mut self, recording_id: u64) -> Result<Box<dyn ActiveCapture>, BoundaryError> {
        if microphone_blocked() {
            return Err(blocked_error(
                "microphone access is denied in privacy settings",
            ));
        }
        let shared = Arc::new(Shared {
            pipeline: Mutex::new(Pipeline::new(Arc::clone(&self.levels))),
            wake: Notify::new(),
        });
        let (control_tx, control_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread_shared = Arc::clone(&shared);
        let thread_control = control_tx.clone();
        let worker = thread::Builder::new()
            .name(format!("voisu-capture-{recording_id}"))
            .spawn(move || run_capture(thread_shared, thread_control, control_rx, ready_tx))
            .map_err(|error| {
                BoundaryError::new(
                    BoundaryKind::Capture,
                    format!("capture thread unavailable: {error}"),
                )
            })?;
        let opened = ready_rx.recv_timeout(OPEN_TIMEOUT).unwrap_or_else(|_| {
            Err(Failure::new(
                BoundaryKind::Capture,
                "microphone did not open in time".into(),
                MIC_FAILED,
            ))
        });
        if let Err(failure) = opened {
            let _ = control_tx.send(Control::Stop);
            return Err(failure.into_error());
        }
        tracing::info!(recording_id, "capture started");
        Ok(Box::new(CpalActiveCapture {
            shared,
            control: control_tx,
            worker: Some(worker),
            started: Instant::now(),
            pcm: Vec::new(),
        }))
    }
}

struct CpalActiveCapture {
    shared: Arc<Shared>,
    control: mpsc::Sender<Control>,
    worker: Option<thread::JoinHandle<()>>,
    started: Instant,
    pcm: Vec<u8>,
}

impl CpalActiveCapture {
    /// Stops the stream, flushes the resampler tail into the queue, and waits
    /// (bounded) for the capture thread to exit.
    async fn stop(&mut self) {
        let _ = self.control.send(Control::Stop);
        if let Some(worker) = self.worker.take() {
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
        let _ = self.control.send(Control::Stop);
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

fn blocked_error(diagnostic: &str) -> BoundaryError {
    BoundaryError::new(BoundaryKind::Capture, diagnostic).with_public_message(MIC_BLOCKED)
}

struct Shared {
    pipeline: Mutex<Pipeline>,
    wake: Notify,
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
    ready: mpsc::SyncSender<Result<(), Failure>>,
) {
    let mut stream = match open_stream(&shared, &control_tx) {
        Ok(stream) => {
            let _ = ready.send(Ok(()));
            stream
        }
        Err(failure) => {
            let _ = ready.send(Err(failure));
            return;
        }
    };
    loop {
        match control_rx.recv() {
            Ok(Control::Stop) | Err(_) => break,
            Ok(Control::Fatal(failure)) => {
                drop(stream);
                return fail_pipeline(&shared, failure);
            }
            Ok(Control::Reopen) => {
                drop(stream);
                match reopen(&shared, &control_tx, &control_rx) {
                    Reopened::Stream(next) => stream = next,
                    Reopened::Stopped => return finish_pipeline(&shared),
                    Reopened::Failed(failure) => return fail_pipeline(&shared, failure),
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
    shared: &Arc<Shared>,
    control_tx: &mpsc::Sender<Control>,
    control_rx: &mpsc::Receiver<Control>,
) -> Reopened {
    let deadline = Instant::now() + REPLACEMENT_WAIT;
    loop {
        match open_stream(shared, control_tx) {
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
            Ok(Control::Reopen | Control::Fatal(_)) | Err(RecvTimeoutError::Timeout) => {}
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

fn fail_pipeline(shared: &Shared, failure: Failure) {
    tracing::warn!(diagnostic = %failure.diagnostic, "capture failed");
    shared.pipeline.lock().unwrap().failure = Some(failure);
    shared.wake.notify_one();
}

fn open_stream(
    shared: &Arc<Shared>,
    control_tx: &mpsc::Sender<Control>,
) -> Result<cpal::Stream, Failure> {
    let host = cpal::default_host();
    let device = host.default_input_device().ok_or_else(|| {
        Failure::new(
            BoundaryKind::Capture,
            "no default input device".into(),
            NO_MIC,
        )
    })?;
    let supported = device
        .default_input_config()
        .map_err(|error| classify("default input config unavailable", &error))?;
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
    let stream = match format {
        SampleFormat::F32 => build::<f32>(&device, config, shared, control_tx),
        SampleFormat::F64 => build::<f64>(&device, config, shared, control_tx),
        SampleFormat::I16 => build::<i16>(&device, config, shared, control_tx),
        SampleFormat::I32 => build::<i32>(&device, config, shared, control_tx),
        SampleFormat::U16 => build::<u16>(&device, config, shared, control_tx),
        SampleFormat::U8 => build::<u8>(&device, config, shared, control_tx),
        SampleFormat::I8 => build::<i8>(&device, config, shared, control_tx),
        other => {
            return Err(Failure::new(
                BoundaryKind::Capture,
                format!("unsupported sample format {other:?}"),
                MIC_FAILED,
            ));
        }
    }
    .map_err(|error| classify("input stream unavailable", &error))?;
    stream
        .play()
        .map_err(|error| classify("input stream did not start", &error))?;
    Ok(stream)
}

fn build<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    shared: &Arc<Shared>,
    control_tx: &mpsc::Sender<Control>,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    let data_shared = Arc::clone(shared);
    let mut converted: Vec<f32> = Vec::new();
    let errors = control_tx.clone();
    // Once the stream has failed, later callbacks are ignored.
    let mut reported = false;
    device.build_input_stream::<T, _, _>(
        config,
        move |data: &[T], _| {
            converted.clear();
            converted.extend(
                data.iter()
                    .map(|&sample| <f32 as FromSample<T>>::from_sample_(sample)),
            );
            data_shared.pipeline.lock().unwrap().push(&converted);
            data_shared.wake.notify_one();
        },
        move |error: cpal::Error| {
            if reported {
                return;
            }
            let control = match error.kind() {
                // Glitches and refused real-time priority: keep recording.
                // `DeviceChanged` means cpal already rerouted the stream; a
                // WASAPI default-mic switch arrives as `StreamInvalidated`.
                ErrorKind::Xrun | ErrorKind::RealtimeDenied | ErrorKind::DeviceChanged => return,
                ErrorKind::StreamInvalidated | ErrorKind::DeviceNotAvailable => Control::Reopen,
                _ => Control::Fatal(classify("input stream failed", &error)),
            };
            reported = true;
            let _ = errors.send(control);
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

#[cfg(not(windows))]
fn microphone_blocked() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn pipeline() -> (Pipeline, Arc<AtomicUsize>) {
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
