//! Keep microphone ready: a stream that stays open between Recordings, so a
//! press records at once instead of after the ~0.8 s WASAPI takes to build
//! one, and the half second before the press is kept.
//!
//! One keeper thread owns the stream (a cpal `Stream` is not `Send`). Its
//! callback writes into a [`Tap`]: a fixed ring of the last
//! [`PREROLL`] while no Recording is attached, the Recording's pipeline
//! while one is. The ring lives in memory only and is overwritten as it
//! goes; only the Recording a press attaches receives it. When to open,
//! close and rebuild is decided by [`Policy`], which is pure so it can be
//! tested with a fake clock.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use cpal::traits::StreamTrait;

use super::{
    Failure, NO_MIC, REPLACEMENT_POLL, REPLACEMENT_WAIT, ResolvedInput, Shared, StreamEvent,
    build_stream, microphone_blocked,
};
use crate::settings::KeepMicReady;

/// Audio before the press that the Recording keeps.
const PREROLL: Duration = Duration::from_millis(500);
/// For 5 minutes after each use.
const LINGER: Duration = Duration::from_secs(5 * 60);
/// A press attaches only to a stream that delivered audio this recently;
/// WASAPI calls back every 10 ms.
const LIVE_WINDOW: Duration = Duration::from_millis(300);
/// How often the keeper checks that an open stream still delivers audio.
const HEALTH_CHECK: Duration = Duration::from_secs(1);
/// An open stream silent this long has died (sleep, a driver reset) and is
/// rebuilt.
const STALL_AFTER: Duration = Duration::from_secs(2);
/// The longest a press waits to confirm the ready stream is on the current
/// default mic before it opens one itself.
const VERIFY_TIMEOUT: Duration = Duration::from_millis(200);
const RETRY_FIRST: Duration = Duration::from_secs(2);
const RETRY_MAX: Duration = Duration::from_secs(60);

/// What the keeper should do to the stream, and why (for the log).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Command {
    Open(&'static str),
    Close(&'static str),
    Rebuild(&'static str),
}

/// When the ready stream should exist. Every deadline it reports is cleared
/// by the [`Policy::tick`] that reaches it, so the keeper never spins.
#[derive(Debug)]
pub(super) struct Policy {
    mode: KeepMicReady,
    open: bool,
    /// A Recording is attached to the open stream.
    in_use: bool,
    linger_until: Option<Instant>,
    retry_at: Option<Instant>,
    failures: u32,
}

impl Policy {
    pub(super) fn new(mode: KeepMicReady) -> Self {
        Self {
            mode,
            open: false,
            in_use: false,
            linger_until: None,
            retry_at: None,
            failures: 0,
        }
    }

    pub(super) fn mode(&self) -> KeepMicReady {
        self.mode
    }

    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    fn wants_open(&self, now: Instant) -> bool {
        match self.mode {
            KeepMicReady::WhileRunning => true,
            KeepMicReady::ForFiveMinutes => {
                self.in_use || self.linger_until.is_some_and(|until| now < until)
            }
            KeepMicReady::Off => false,
        }
    }

    fn close(&mut self, reason: &'static str) -> Option<Command> {
        self.open = false;
        self.linger_until = None;
        Some(Command::Close(reason))
    }

    pub(super) fn start(&mut self, _now: Instant) -> Option<Command> {
        (self.mode == KeepMicReady::WhileRunning).then_some(Command::Open("app started"))
    }

    pub(super) fn set_mode(&mut self, mode: KeepMicReady, now: Instant) -> Option<Command> {
        if mode == self.mode {
            return None;
        }
        self.mode = mode;
        self.retry_at = None;
        self.failures = 0;
        match mode {
            KeepMicReady::WhileRunning => {
                self.linger_until = None;
                (!self.open).then_some(Command::Open("setting changed"))
            }
            KeepMicReady::ForFiveMinutes => {
                // Kept for five minutes from now; closed stays closed until
                // the next Recording ends.
                self.linger_until = (self.open && !self.in_use).then(|| now + LINGER);
                None
            }
            // A Recording using the stream keeps it until it ends.
            KeepMicReady::Off if self.open && !self.in_use => self.close("setting turned off"),
            KeepMicReady::Off => {
                self.linger_until = None;
                None
            }
        }
    }

    /// A press attached a Recording to the open stream.
    pub(super) fn attached(&mut self) {
        self.in_use = true;
        self.linger_until = None;
    }

    /// A Recording's capture ended, whether it used the ready stream or
    /// opened its own.
    pub(super) fn released(&mut self, now: Instant) -> Option<Command> {
        self.in_use = false;
        match self.mode {
            KeepMicReady::Off if self.open => self.close("setting turned off"),
            KeepMicReady::Off => None,
            KeepMicReady::ForFiveMinutes | KeepMicReady::WhileRunning => {
                if self.mode == KeepMicReady::ForFiveMinutes {
                    self.linger_until = Some(now + LINGER);
                }
                if self.open {
                    return None;
                }
                // A Recording worked or failed in front of the user: try
                // again now rather than at the back of a long backoff.
                self.retry_at = None;
                self.failures = 0;
                Some(Command::Open("Recording ended"))
            }
        }
    }

    pub(super) fn opened(&mut self) {
        self.open = true;
        self.retry_at = None;
    }

    /// The open stream is delivering audio. Only this ends the backoff: a
    /// stream that opens but stays silent keeps backing off.
    pub(super) fn healthy(&mut self) {
        self.failures = 0;
    }

    /// The stream could not open, or died. Returns the wait before the next
    /// attempt, if one is due.
    pub(super) fn failed(&mut self, now: Instant) -> Option<Duration> {
        self.open = false;
        self.in_use = false;
        if !self.wants_open(now) {
            self.retry_at = None;
            return None;
        }
        let delay = RETRY_FIRST
            .saturating_mul(1 << self.failures.min(5))
            .min(RETRY_MAX);
        self.failures = self.failures.saturating_add(1);
        self.retry_at = Some(now + delay);
        Some(delay)
    }

    /// The stream's device went away or the default mic changed.
    pub(super) fn lost(&mut self, reason: &'static str) -> Option<Command> {
        if self.open {
            return Some(Command::Rebuild(reason));
        }
        // A mic arriving ends the wait for a retry.
        self.retry_at.take().map(|_| Command::Open(reason))
    }

    pub(super) fn tick(&mut self, now: Instant) -> Option<Command> {
        if self.open {
            if self.in_use {
                return None;
            }
            if self.mode == KeepMicReady::Off {
                return self.close("setting turned off");
            }
            if self.mode == KeepMicReady::ForFiveMinutes
                && self.linger_until.is_none_or(|until| now >= until)
            {
                return self.close("idle for 5 minutes");
            }
            return None;
        }
        let due = self.retry_at.is_some_and(|at| now >= at);
        if !due {
            return None;
        }
        self.retry_at = None;
        self.wants_open(now).then_some(Command::Open("retry"))
    }

    /// The next instant [`Policy::tick`] has work at.
    pub(super) fn deadline(&self) -> Option<Instant> {
        if self.open {
            return if self.in_use || self.mode != KeepMicReady::ForFiveMinutes {
                None
            } else {
                self.linger_until
            };
        }
        self.retry_at
    }
}

/// The last [`PREROLL`] of interleaved device samples. Allocated when a
/// stream opens; writing never allocates.
#[derive(Default)]
pub(super) struct PreRoll {
    samples: Vec<f32>,
    start: usize,
    len: usize,
}

impl PreRoll {
    /// Holds `frames` frames of `channels` samples, emptied.
    pub(super) fn reset(&mut self, frames: usize, channels: usize) {
        let capacity = frames * channels.max(1);
        self.samples.clear();
        self.samples.resize(capacity, 0.0);
        self.start = 0;
        self.len = 0;
    }

    /// Appends whole frames, overwriting the oldest once full.
    pub(super) fn write(&mut self, data: &[f32]) {
        let capacity = self.samples.len();
        if capacity == 0 {
            return;
        }
        let data = &data[data.len().saturating_sub(capacity)..];
        let end = (self.start + self.len) % capacity;
        let first = data.len().min(capacity - end);
        self.samples[end..end + first].copy_from_slice(&data[..first]);
        self.samples[..data.len() - first].copy_from_slice(&data[first..]);
        let len = self.len + data.len();
        if len > capacity {
            self.start = (self.start + len - capacity) % capacity;
            self.len = capacity;
        } else {
            self.len = len;
        }
    }

    /// Hands the held samples to `out` oldest first, then empties the ring.
    /// Returns how many samples it handed over.
    pub(super) fn drain(&mut self, mut out: impl FnMut(&[f32])) -> usize {
        let capacity = self.samples.len();
        let held = self.len;
        if held > 0 {
            let first_end = (self.start + held).min(capacity);
            out(&self.samples[self.start..first_end]);
            let wrapped = (self.start + held).saturating_sub(capacity);
            if wrapped > 0 {
                out(&self.samples[..wrapped]);
            }
        }
        self.start = 0;
        self.len = 0;
        held
    }

    fn clear(&mut self) {
        self.start = 0;
        self.len = 0;
    }
}

/// What the ready stream's callback, the keeper and a press share.
#[derive(Default)]
struct Tap {
    state: Mutex<TapState>,
}

#[derive(Default)]
struct TapState {
    /// The stream whose callbacks count; older ones are ignored.
    attempt: u64,
    /// Set once the stream plays.
    live: Option<Live>,
    last_audio: Option<Instant>,
    preroll: PreRoll,
    recording: Option<Arc<Shared>>,
}

struct Live {
    id: cpal::DeviceId,
    rate: u32,
    channels: usize,
    opened: Instant,
}

impl Tap {
    /// The audio callback: one uncontended lock, no allocation.
    fn deliver(&self, attempt: u64, samples: &[f32]) {
        let mut state = self.state.lock().unwrap();
        if state.attempt != attempt {
            return;
        }
        state.last_audio = Some(Instant::now());
        match &state.recording {
            Some(shared) => {
                let mut pipeline = shared.pipeline.lock().unwrap();
                if shared.stop.requested() {
                    return;
                }
                pipeline.push(samples);
                drop(pipeline);
                shared.wake.notify_one();
            }
            None => state.preroll.write(samples),
        }
    }

    /// Retires the current stream's callbacks before a new one is built or
    /// the stream is dropped.
    fn retire(&self) -> u64 {
        let mut state = self.state.lock().unwrap();
        state.attempt += 1;
        state.live = None;
        state.last_audio = None;
        state.preroll.clear();
        state.attempt
    }
}

enum Msg {
    Mode(KeepMicReady),
    Attached,
    Released,
    DefaultChanged,
    Stream(u64, StreamEvent),
}

/// The handle to the ready stream: the press, the settings command and the
/// default-mic watcher each hold a clone.
#[derive(Clone)]
pub(crate) struct ReadyMic {
    tap: Arc<Tap>,
    /// The setting as last applied, for the per-Recording log.
    mode: Arc<Mutex<KeepMicReady>>,
    events: mpsc::Sender<Msg>,
}

/// Why a press opened the mic itself.
pub(super) type Cold = &'static str;

impl ReadyMic {
    pub(super) fn start(mode: KeepMicReady) -> Self {
        // The macOS build shares this crate; the setting is Windows only.
        let mode = if cfg!(target_os = "macos") {
            KeepMicReady::Off
        } else {
            mode
        };
        let (events, requests) = mpsc::channel();
        let ready = Self {
            tap: Arc::default(),
            mode: Arc::new(Mutex::new(mode)),
            events,
        };
        let keeper = ready.clone();
        let spawned = thread::Builder::new()
            .name("voisu-ready-mic".into())
            .spawn(move || run_keeper(mode, &keeper, &requests));
        if let Err(error) = spawned {
            tracing::warn!(%error, "ready microphone thread could not start");
        }
        ready
    }

    /// Applies a changed setting now; Off closes the stream at once unless
    /// a Recording is using it.
    pub(crate) fn set_mode(&self, mode: KeepMicReady) {
        if !cfg!(target_os = "macos") {
            *self.mode.lock().unwrap() = mode;
            let _ = self.events.send(Msg::Mode(mode));
        }
    }

    pub(super) fn default_changed(&self) {
        let _ = self.events.send(Msg::DefaultChanged);
    }

    /// A Recording's capture ended.
    pub(super) fn released(&self) {
        let _ = self.events.send(Msg::Released);
    }

    /// Attaches the Recording to the ready stream and hands it the pre-roll.
    /// Returns the pre-roll's length, or why the press must open the mic
    /// itself.
    pub(super) async fn attach(&self, shared: &Arc<Shared>) -> Result<Duration, Cold> {
        let (attempt, id) = self.live()?;
        // #348's revalidation: never record from a mic that stopped being
        // the default before the watcher said so.
        let current = tokio::time::timeout(
            VERIFY_TIMEOUT,
            tokio::task::spawn_blocking(move || ResolvedInput::default_id(&cpal::default_host())),
        )
        .await;
        match current {
            Ok(Ok(Some(current))) if current == id => {}
            Ok(Ok(_)) => {
                self.default_changed();
                return Err("default microphone changed");
            }
            Ok(Err(_)) | Err(_) => return Err("default microphone check timed out"),
        }
        self.join(shared, attempt)
    }

    /// The stream a press may attach to, if it is delivering audio.
    fn live(&self) -> Result<(u64, cpal::DeviceId), Cold> {
        let state = self.tap.state.lock().unwrap();
        let Some(live) = &state.live else {
            return Err(if *self.mode.lock().unwrap() == KeepMicReady::Off {
                "setting is off"
            } else {
                "not ready"
            });
        };
        let fresh = state
            .last_audio
            .is_some_and(|at| at.elapsed() < LIVE_WINDOW);
        if !fresh {
            return Err("ready stream silent");
        }
        Ok((state.attempt, live.id.clone()))
    }

    /// Hands the pre-roll to `shared` and routes the stream's audio to it,
    /// unless the stream was rebuilt since [`ReadyMic::live`].
    fn join(&self, shared: &Arc<Shared>, attempt: u64) -> Result<Duration, Cold> {
        let mut state = self.tap.state.lock().unwrap();
        let Some(live) = state.live.as_ref().filter(|_| state.attempt == attempt) else {
            return Err("ready stream rebuilt");
        };
        let (rate, channels) = (live.rate, live.channels);
        let samples = {
            let mut pipeline = shared.pipeline.lock().unwrap();
            pipeline.reformat(rate, channels);
            state.preroll.drain(|held| pipeline.push(held))
        };
        state.recording = Some(Arc::clone(shared));
        drop(state);
        shared.wake.notify_one();
        let _ = self.events.send(Msg::Attached);
        let frames = (samples / channels.max(1)) as u64;
        Ok(Duration::from_micros(
            frames * 1_000_000 / u64::from(rate.max(1)),
        ))
    }

    /// Stops feeding `shared`; once this returns no more audio reaches it.
    pub(super) fn detach(&self, shared: &Arc<Shared>) {
        let mut state = self.tap.state.lock().unwrap();
        if state
            .recording
            .as_ref()
            .is_some_and(|attached| Arc::ptr_eq(attached, shared))
        {
            state.recording = None;
            state.preroll.clear();
        }
    }
}

/// The keeper thread: carries out the policy's commands on the one stream.
fn run_keeper(mode: KeepMicReady, ready: &ReadyMic, requests: &mpsc::Receiver<Msg>) {
    tracing::info!(mode = mode_name(mode), "keep microphone ready");
    let host = cpal::default_host();
    let mut policy = Policy::new(mode);
    let mut keeper = Keeper {
        ready,
        host: &host,
        stream: None,
        last_failure: None,
    };
    let mut command = policy.start(Instant::now());
    loop {
        if let Some(command) = command.take() {
            keeper.execute(command, &mut policy);
        }
        let now = Instant::now();
        let mut wait = policy
            .deadline()
            .map_or(Duration::MAX, |at| at.saturating_duration_since(now));
        if policy.is_open() {
            wait = wait.min(HEALTH_CHECK);
        }
        let message = if wait == Duration::MAX {
            requests.recv().map_err(|_| RecvTimeoutError::Disconnected)
        } else {
            requests.recv_timeout(wait)
        };
        let now = Instant::now();
        let reaction = match message {
            Ok(Msg::Mode(mode)) => {
                if mode != policy.mode() {
                    tracing::info!(mode = mode_name(mode), "keep microphone ready changed");
                }
                policy.set_mode(mode, now)
            }
            Ok(Msg::Attached) => {
                policy.attached();
                None
            }
            Ok(Msg::Released) => policy.released(now),
            Ok(Msg::DefaultChanged) => policy.lost("default microphone changed"),
            Ok(Msg::Stream(attempt, _)) if !keeper.is_current(attempt) => None,
            Ok(Msg::Stream(_, StreamEvent::Audio)) => None,
            Ok(Msg::Stream(_, StreamEvent::Reopen)) => policy.lost("microphone went away"),
            Ok(Msg::Stream(_, StreamEvent::Fatal(failure))) => {
                keeper.fail(failure, &mut policy);
                None
            }
            Err(RecvTimeoutError::Timeout) => {
                if keeper.delivering() {
                    policy.healthy();
                }
                keeper.stalled().and_then(|silent_since_open| {
                    if silent_since_open {
                        // Never delivered: treat as a failed open, backing off.
                        keeper.fail(
                            Failure::new(
                                voisu_core::BoundaryKind::Capture,
                                "ready microphone produced no audio".into(),
                                super::MIC_FAILED,
                            ),
                            &mut policy,
                        );
                        None
                    } else {
                        policy.lost("microphone stopped delivering audio")
                    }
                })
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        // A deadline that passed while messages kept arriving still fires.
        command = reaction.or_else(|| policy.tick(now));
    }
    keeper.close("app closing");
}

struct Keeper<'a> {
    ready: &'a ReadyMic,
    host: &'a cpal::Host,
    stream: Option<cpal::Stream>,
    /// The last failure logged, so a mic that stays blocked or missing logs
    /// once, not on every retry.
    last_failure: Option<String>,
}

impl Keeper<'_> {
    fn tap(&self) -> &Tap {
        &self.ready.tap
    }

    fn is_current(&self, attempt: u64) -> bool {
        self.tap().state.lock().unwrap().attempt == attempt
    }

    fn delivering(&self) -> bool {
        self.stream.is_some()
            && self
                .tap()
                .state
                .lock()
                .unwrap()
                .last_audio
                .is_some_and(|at| at.elapsed() < STALL_AFTER)
    }

    /// `Some(true)` when the open stream never delivered audio within
    /// [`STALL_AFTER`], `Some(false)` when it stopped delivering, `None`
    /// when it is healthy or closed.
    fn stalled(&self) -> Option<bool> {
        self.stream.as_ref()?;
        let state = self.tap().state.lock().unwrap();
        let live = state.live.as_ref()?;
        let since = state.last_audio.unwrap_or(live.opened);
        (since.elapsed() >= STALL_AFTER).then_some(state.last_audio.is_none())
    }

    fn execute(&mut self, command: Command, policy: &mut Policy) {
        match command {
            Command::Close(reason) => self.close(reason),
            Command::Open(reason) | Command::Rebuild(reason) => self.open(reason, policy),
        }
    }

    fn close(&mut self, reason: &'static str) {
        self.ready.tap.retire();
        if self.stream.take().is_some() {
            tracing::info!(reason, "ready microphone closed");
        }
    }

    fn open(&mut self, reason: &'static str, policy: &mut Policy) {
        let rebuilding = self.stream.is_some();
        // Dropped before the next is built: WASAPI need not hold two.
        self.ready.tap.retire();
        drop(self.stream.take());
        let attached = self.tap().state.lock().unwrap().recording.is_some();
        // A Recording carrying on waits briefly for a replacement mic (a
        // headset reconnecting), as a cold Recording does.
        let deadline = Instant::now()
            + if attached {
                REPLACEMENT_WAIT
            } else {
                Duration::ZERO
            };
        let opened = loop {
            match self.try_open() {
                Err(failure) if failure.public == NO_MIC && Instant::now() < deadline => {
                    thread::sleep(REPLACEMENT_POLL);
                }
                result => break result,
            }
        };
        match opened {
            Ok((stream, built_ms)) => {
                self.stream = Some(stream);
                self.last_failure = None;
                policy.opened();
                let state = self.tap().state.lock().unwrap();
                let live = state.live.as_ref();
                tracing::info!(
                    reason,
                    rebuilt = rebuilding,
                    stream_built_ms = built_ms,
                    rate = live.map(|live| live.rate),
                    channels = live.map(|live| live.channels),
                    "ready microphone open"
                );
            }
            Err(failure) => self.fail(failure, policy),
        }
    }

    /// Drops the stream, fails any attached Recording and backs off.
    fn fail(&mut self, failure: Failure, policy: &mut Policy) {
        let recording = {
            let mut state = self.tap().state.lock().unwrap();
            state.recording.take()
        };
        self.ready.tap.retire();
        drop(self.stream.take());
        if let Some(shared) = recording {
            shared.pipeline.lock().unwrap().failure = Some(failure.clone());
            shared.wake.notify_one();
        }
        let retry = policy.failed(Instant::now());
        let retry_in_s = retry.map(|delay| delay.as_secs());
        if self.last_failure.as_deref() == Some(failure.diagnostic.as_str()) {
            tracing::debug!(diagnostic = %failure.diagnostic, ?retry_in_s, "ready microphone still unavailable");
        } else {
            tracing::warn!(
                diagnostic = %failure.diagnostic,
                ?retry_in_s,
                "ready microphone unavailable; presses open the microphone on demand"
            );
            self.last_failure = Some(failure.diagnostic);
        }
    }

    /// Builds and plays a stream on the current default mic. Returns it and
    /// how long the build took.
    fn try_open(&self) -> Result<(cpal::Stream, u128), Failure> {
        if microphone_blocked() {
            return Err(Failure::new(
                voisu_core::BoundaryKind::Capture,
                "microphone access is denied in privacy settings".into(),
                super::MIC_BLOCKED,
            ));
        }
        let begun = Instant::now();
        let input = ResolvedInput::resolve(self.host)?;
        let supported = &input.config;
        let format = supported.sample_format();
        let config = cpal::StreamConfig {
            channels: supported.channels(),
            sample_rate: supported.sample_rate(),
            buffer_size: cpal::BufferSize::Default,
        };
        let rate = config.sample_rate;
        let channels = usize::from(config.channels);
        let attempt = {
            let mut state = self.tap().state.lock().unwrap();
            // Allocated here, never in the callback.
            let frames = (u128::from(rate) * PREROLL.as_millis() / 1000) as usize;
            state.preroll.reset(frames, channels);
            state.attempt
        };
        let tap = Arc::clone(&self.ready.tap);
        let events = self.ready.events.clone();
        let stream = build_stream(
            &input.device,
            config,
            format,
            move |samples| tap.deliver(attempt, samples),
            move |event| {
                let _ = events.send(Msg::Stream(attempt, event));
            },
        )?;
        let built_ms = begun.elapsed().as_millis();
        input.verify_default(self.host)?;
        stream
            .play()
            .map_err(|error| super::classify("input stream did not start", &error))?;
        let mut state = self.tap().state.lock().unwrap();
        if state.attempt != attempt {
            return Err(Failure::new(
                voisu_core::BoundaryKind::Capture,
                "ready stream retired while opening".into(),
                super::MIC_FAILED,
            ));
        }
        if let Some(shared) = &state.recording {
            // Carrying a Recording over to the new mic's format.
            shared.pipeline.lock().unwrap().reformat(rate, channels);
        }
        state.live = Some(Live {
            id: input.id,
            rate,
            channels,
            opened: Instant::now(),
        });
        Ok((stream, built_ms))
    }
}

fn mode_name(mode: KeepMicReady) -> &'static str {
    match mode {
        KeepMicReady::WhileRunning => "while_running",
        KeepMicReady::ForFiveMinutes => "for_five_minutes",
        KeepMicReady::Off => "off",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: Duration = Duration::from_secs(60);

    fn opened(mode: KeepMicReady, now: Instant) -> Policy {
        let mut policy = Policy::new(mode);
        policy.opened();
        if mode == KeepMicReady::ForFiveMinutes {
            policy.linger_until = Some(now + LINGER);
        }
        policy
    }

    #[test]
    fn only_while_running_opens_at_app_start() {
        let now = Instant::now();
        let start = |mode| Policy::new(mode).start(now);
        assert_eq!(
            start(KeepMicReady::WhileRunning),
            Some(Command::Open("app started"))
        );
        assert_eq!(start(KeepMicReady::ForFiveMinutes), None);
        assert_eq!(start(KeepMicReady::Off), None);
    }

    #[test]
    fn off_never_opens_and_has_no_deadline() {
        let now = Instant::now();
        let mut policy = Policy::new(KeepMicReady::Off);
        assert_eq!(policy.released(now), None);
        assert_eq!(policy.lost("default microphone changed"), None);
        assert_eq!(policy.tick(now + 10 * MIN), None);
        assert_eq!(policy.deadline(), None);
    }

    #[test]
    fn five_minutes_opens_after_a_recording_and_closes_five_minutes_later() {
        let t0 = Instant::now();
        let mut policy = Policy::new(KeepMicReady::ForFiveMinutes);
        assert_eq!(policy.start(t0), None);
        assert_eq!(policy.released(t0), Some(Command::Open("Recording ended")));
        policy.opened();
        assert_eq!(policy.deadline(), Some(t0 + LINGER));
        assert_eq!(policy.tick(t0 + 4 * MIN), None);
        assert_eq!(
            policy.tick(t0 + LINGER),
            Some(Command::Close("idle for 5 minutes"))
        );
        assert!(!policy.is_open());
        assert_eq!(policy.deadline(), None);
    }

    #[test]
    fn each_use_restarts_the_five_minutes_and_a_long_recording_is_never_cut() {
        let t0 = Instant::now();
        let mut policy = opened(KeepMicReady::ForFiveMinutes, t0);
        policy.attached();
        // Ten minutes recording: the stream stays.
        assert_eq!(policy.deadline(), None);
        assert_eq!(policy.tick(t0 + 10 * MIN), None);
        let ended = t0 + 10 * MIN;
        assert_eq!(policy.released(ended), None);
        assert_eq!(policy.deadline(), Some(ended + LINGER));
        assert_eq!(policy.tick(ended + 4 * MIN), None);
        assert!(policy.tick(ended + LINGER).is_some());
    }

    #[test]
    fn turning_off_closes_now_or_when_the_recording_ends() {
        let now = Instant::now();
        let mut idle = opened(KeepMicReady::WhileRunning, now);
        assert_eq!(
            idle.set_mode(KeepMicReady::Off, now),
            Some(Command::Close("setting turned off"))
        );
        assert!(!idle.is_open());

        let mut busy = opened(KeepMicReady::WhileRunning, now);
        busy.attached();
        assert_eq!(busy.set_mode(KeepMicReady::Off, now), None);
        assert_eq!(busy.tick(now), None, "the Recording keeps its mic");
        assert_eq!(
            busy.released(now),
            Some(Command::Close("setting turned off"))
        );
    }

    #[test]
    fn switching_modes_applies_at_once() {
        let now = Instant::now();
        let mut policy = Policy::new(KeepMicReady::Off);
        assert_eq!(
            policy.set_mode(KeepMicReady::WhileRunning, now),
            Some(Command::Open("setting changed"))
        );
        assert_eq!(policy.set_mode(KeepMicReady::WhileRunning, now), None);
        policy.opened();
        assert_eq!(policy.set_mode(KeepMicReady::ForFiveMinutes, now), None);
        assert_eq!(policy.deadline(), Some(now + LINGER));
        assert_eq!(
            policy.set_mode(KeepMicReady::WhileRunning, now),
            None,
            "already open"
        );
        assert_eq!(policy.deadline(), None, "while running never times out");
    }

    #[test]
    fn a_lost_stream_is_rebuilt_and_a_failed_one_backs_off() {
        let t0 = Instant::now();
        let mut policy = opened(KeepMicReady::WhileRunning, t0);
        assert_eq!(
            policy.lost("microphone went away"),
            Some(Command::Rebuild("microphone went away"))
        );
        let delays: Vec<_> = (0..8).map(|_| policy.failed(t0).unwrap()).collect();
        assert_eq!(delays[0], RETRY_FIRST);
        assert!(delays.windows(2).all(|pair| pair[0] <= pair[1]));
        assert_eq!(*delays.last().unwrap(), RETRY_MAX);
        // Nothing happens before the retry is due, then exactly one open.
        let due = policy.deadline().unwrap();
        assert_eq!(due, t0 + RETRY_MAX);
        assert_eq!(policy.tick(due - Duration::from_millis(1)), None);
        assert_eq!(policy.tick(due), Some(Command::Open("retry")));
        assert_eq!(policy.deadline(), None);
        policy.opened();
        assert_eq!(
            policy.failed(due),
            Some(RETRY_MAX),
            "a stream that opens but never delivers keeps backing off"
        );
        policy.tick(due + RETRY_MAX);
        policy.opened();
        policy.healthy();
        assert_eq!(policy.failed(due), Some(RETRY_FIRST), "audio resets it");
    }

    #[test]
    fn a_new_default_mic_or_a_recording_ends_the_backoff() {
        let now = Instant::now();
        let mut policy = Policy::new(KeepMicReady::WhileRunning);
        policy.failed(now);
        assert_eq!(
            policy.lost("default microphone changed"),
            Some(Command::Open("default microphone changed"))
        );
        policy.failed(now);
        assert_eq!(policy.released(now), Some(Command::Open("Recording ended")));
    }

    #[test]
    fn five_minutes_stops_retrying_once_its_window_has_passed() {
        let t0 = Instant::now();
        let mut policy = Policy::new(KeepMicReady::ForFiveMinutes);
        policy.released(t0);
        policy.failed(t0);
        let late = t0 + LINGER + MIN;
        assert_eq!(policy.tick(late), None);
        assert_eq!(policy.deadline(), None);
        assert_eq!(policy.failed(late), None);
    }

    #[test]
    fn every_deadline_is_cleared_by_the_tick_that_reaches_it() {
        let t0 = Instant::now();
        let modes = [
            KeepMicReady::WhileRunning,
            KeepMicReady::ForFiveMinutes,
            KeepMicReady::Off,
        ];
        for mode in modes {
            let mut policy = Policy::new(mode);
            let mut now = t0;
            policy.released(now);
            policy.failed(now);
            for step in 0..50 {
                let Some(at) = policy.deadline() else { break };
                assert!(at > now || step == 0, "{mode:?} deadline did not advance");
                now = at;
                if let Some(Command::Open(_)) = policy.tick(now) {
                    policy.failed(now);
                }
            }
        }
    }

    fn ordered(preroll: &mut PreRoll) -> Vec<f32> {
        let mut out = Vec::new();
        preroll.drain(|held| out.extend_from_slice(held));
        out
    }

    #[test]
    fn preroll_keeps_the_newest_samples_in_order() {
        let mut preroll = PreRoll::default();
        preroll.reset(4, 2);
        preroll.write(&[1.0, 1.0, 2.0, 2.0]);
        preroll.write(&[3.0, 3.0, 4.0, 4.0, 5.0, 5.0]);
        assert_eq!(
            ordered(&mut preroll),
            [2.0, 2.0, 3.0, 3.0, 4.0, 4.0, 5.0, 5.0]
        );
    }

    #[test]
    fn preroll_is_handed_over_once() {
        let mut preroll = PreRoll::default();
        preroll.reset(3, 1);
        preroll.write(&[1.0, 2.0]);
        assert_eq!(ordered(&mut preroll), [1.0, 2.0]);
        assert!(ordered(&mut preroll).is_empty(), "drained");
        preroll.write(&[3.0]);
        assert_eq!(ordered(&mut preroll), [3.0]);
    }

    #[test]
    fn a_buffer_longer_than_the_preroll_keeps_its_tail() {
        let mut preroll = PreRoll::default();
        preroll.reset(3, 1);
        preroll.write(&[9.0]);
        preroll.write(&[1.0, 2.0, 3.0, 4.0, 5.0]);
        assert_eq!(ordered(&mut preroll), [3.0, 4.0, 5.0]);
    }

    #[test]
    fn writing_never_reallocates_the_ring() {
        let mut preroll = PreRoll::default();
        preroll.reset(24_000, 2);
        let before = (preroll.samples.as_ptr(), preroll.samples.capacity());
        let buffer = vec![0.5f32; 960];
        for _ in 0..200 {
            preroll.write(&buffer);
        }
        assert_eq!(
            (preroll.samples.as_ptr(), preroll.samples.capacity()),
            before
        );
        assert_eq!(ordered(&mut preroll).len(), 48_000);
    }

    fn ready_mic() -> (ReadyMic, mpsc::Receiver<Msg>) {
        let (events, requests) = mpsc::channel();
        let ready = ReadyMic {
            tap: Arc::default(),
            mode: Arc::new(Mutex::new(KeepMicReady::WhileRunning)),
            events,
        };
        (ready, requests)
    }

    fn go_live(ready: &ReadyMic, rate: u32, channels: usize) {
        let mut state = ready.tap.state.lock().unwrap();
        state.preroll.reset(rate as usize / 2, channels);
        state.live = Some(Live {
            id: cpal::DeviceId::new(cpal::default_host().id(), "test mic"),
            rate,
            channels,
            opened: Instant::now(),
        });
        state.last_audio = Some(Instant::now());
    }

    fn recorded_bytes(shared: &Shared) -> usize {
        let pipeline = shared.pipeline.lock().unwrap();
        pipeline.chunks.iter().map(|c| c.0.len()).sum::<usize>() + pipeline.partial.len()
    }

    #[test]
    fn a_press_gets_the_preroll_first_then_live_audio_until_it_stops() {
        let (capture, _commands) =
            super::super::tests::capture_with_pipeline(super::super::tests::pipeline().0);
        let shared = Arc::clone(&capture.shared);
        let (ready, requests) = ready_mic();
        go_live(&ready, 16_000, 1);
        // Before the press, outside any Recording: only the ring keeps it.
        ready.tap.deliver(0, &[0.5; 4_000]);
        assert_eq!(recorded_bytes(&shared), 0);

        let (attempt, _) = ready.live().unwrap();
        let preroll = ready.join(&shared, attempt).unwrap();
        assert_eq!(preroll, Duration::from_millis(250));
        assert!(matches!(requests.try_recv(), Ok(Msg::Attached)));
        // 16 kHz mono passes through unresampled: two bytes a sample.
        assert_eq!(recorded_bytes(&shared), 4_000 * 2);

        ready.tap.deliver(0, &[0.5; 1_600]);
        assert_eq!(recorded_bytes(&shared), 5_600 * 2);
        ready.detach(&shared);
        ready.tap.deliver(0, &[0.5; 1_600]);
        assert_eq!(recorded_bytes(&shared), 5_600 * 2, "nothing after stop");

        // The next press starts from what was heard after this one ended.
        let (next, _commands) =
            super::super::tests::capture_with_pipeline(super::super::tests::pipeline().0);
        let next = Arc::clone(&next.shared);
        assert_eq!(ready.join(&next, attempt), Ok(Duration::from_millis(100)));
    }

    #[tokio::test]
    async fn a_ready_recording_ends_by_detaching_and_releasing_the_mic() {
        use voisu_core::ActiveCapture;

        let (ready, requests) = ready_mic();
        go_live(&ready, 16_000, 1);
        ready.tap.deliver(0, &[0.5; 4_000]);
        let (pipeline, _) = super::super::tests::pipeline();
        let shared = Arc::new(Shared {
            pipeline: Mutex::new(pipeline),
            wake: tokio::sync::Notify::new(),
            startup: Arc::default(),
            stop: super::super::CaptureStop::default(),
        });
        let (attempt, _) = ready.live().unwrap();
        ready.join(&shared, attempt).unwrap();
        ready.tap.deliver(0, &[0.5; 1_600]);
        let mut capture = super::super::CpalActiveCapture {
            shared: Arc::clone(&shared),
            source: super::super::Source::Ready,
            ready: Some(ready.clone()),
            started: Instant::now(),
            pcm: Vec::new(),
        };
        let audio = capture.finish().await.unwrap();
        assert_eq!(audio.pcm_s16le_mono_16khz().len(), 5_600 * 2);
        assert!(matches!(requests.try_recv(), Ok(Msg::Attached)));
        assert!(matches!(requests.try_recv(), Ok(Msg::Released)));
        drop(capture);
        assert!(requests.try_recv().is_err(), "released once");
        // The stream stays open and goes back to the ring.
        ready.tap.deliver(0, &[0.5; 1_600]);
        assert_eq!(recorded_bytes(&shared), 0);
        assert_eq!(ready.tap.state.lock().unwrap().preroll.len, 1_600);
    }

    #[test]
    fn a_rebuilt_stream_sends_the_press_cold() {
        let (capture, _commands) =
            super::super::tests::capture_with_pipeline(super::super::tests::pipeline().0);
        let (ready, _requests) = ready_mic();
        go_live(&ready, 48_000, 2);
        let (attempt, _) = ready.live().unwrap();
        ready.tap.retire();
        assert_eq!(
            ready.join(&capture.shared, attempt),
            Err("ready stream rebuilt")
        );
        assert!(ready.tap.state.lock().unwrap().recording.is_none());
    }

    #[test]
    fn a_silent_or_closed_stream_sends_the_press_cold() {
        let (ready, _requests) = ready_mic();
        assert_eq!(ready.live().err(), Some("not ready"));
        *ready.mode.lock().unwrap() = KeepMicReady::Off;
        assert_eq!(ready.live().err(), Some("setting is off"));
        go_live(&ready, 48_000, 2);
        ready.tap.state.lock().unwrap().last_audio = None;
        assert_eq!(ready.live().err(), Some("ready stream silent"));
    }
}
