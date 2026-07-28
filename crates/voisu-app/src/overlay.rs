//! Presentation-only state derived from the daemon's public observer response.
//! This module owns no Recording, provider, or Delivery work.

use std::time::{Duration, Instant};

use voisu_core::{DaemonState, OverlayEvent, OverlayOutcome, Response};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OverlayPhase {
    #[default]
    Hidden,
    Recording,
    Processing,
    Success,
    Failure,
    /// Terminal "nothing usable was heard" — deliberately NOT Failure: the
    /// capsule shows calm amber resting bars and a gentle notification, never
    /// red. Detection stays daemon-side (parked quality-gate decision).
    NoSpeech,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OverlayView {
    pub phase: OverlayPhase,
    pub visible_label: &'static str,
    pub accessible_label: &'static str,
}

impl OverlayView {
    pub const HIDDEN: Self = Self {
        phase: OverlayPhase::Hidden,
        visible_label: "",
        accessible_label: "",
    };

    pub fn from_response(response: &Response) -> Self {
        if !response.ok {
            return Self::failure();
        }
        match response.state {
            Some(DaemonState::Recording) => Self {
                phase: OverlayPhase::Recording,
                visible_label: "Recording",
                accessible_label: "Recording; voice activity visible",
            },
            Some(DaemonState::Processing) => Self {
                phase: OverlayPhase::Processing,
                visible_label: "Processing",
                accessible_label: "Processing Recording",
            },
            Some(DaemonState::Idle) | None => Self::HIDDEN,
        }
    }

    pub const fn from_terminal_event(event: &OverlayEvent) -> Self {
        match event.outcome {
            OverlayOutcome::Delivered => Self { phase: OverlayPhase::Success,
                visible_label: "Delivered", accessible_label: "Transcript Delivered" },
            OverlayOutcome::QualityFailure => Self::no_speech(),
            _ => Self { phase: OverlayPhase::Failure,
                visible_label: "Failure", accessible_label: "Recording failed" },
        }
    }

    pub const fn success() -> Self {
        Self { phase: OverlayPhase::Success,
            visible_label: "Delivered", accessible_label: "Transcript Delivered" }
    }

    pub const fn failure() -> Self {
        Self { phase: OverlayPhase::Failure,
            visible_label: "Quality Failure", accessible_label: "Quality Failure" }
    }

    /// The Failure view shown when the optional Overlay cannot reach the
    /// daemon. Owned here so the label strings live in one place; the label
    /// text is load-bearing for tests and users and must stay unchanged.
    pub const fn daemon_unavailable() -> Self {
        Self {
            phase: OverlayPhase::Failure,
            visible_label: "Daemon unavailable",
            accessible_label: "Daemon unavailable; the optional Overlay cannot reach voisu-daemon",
        }
    }

    /// The gentle no-speech terminal view. `visible_label` is what the
    /// notification rung speaks, so it is the full sentence, not a status word.
    pub const fn no_speech() -> Self {
        Self {
            phase: OverlayPhase::NoSpeech,
            visible_label: "Didn't catch any speech",
            accessible_label: "No speech detected; nothing was delivered",
        }
    }

    /// What the GTK capsule's text label shows. Graphics-first phases render
    /// through the bar meter / glyph instead of words; only phases whose meaning
    /// text still carries (Failure, daemon-unavailable) keep their label. The
    /// notification rung keeps using `visible_label` unchanged.
    pub const fn capsule_text(&self) -> &'static str {
        match self.phase {
            OverlayPhase::Recording
            | OverlayPhase::Processing
            | OverlayPhase::Success
            | OverlayPhase::NoSpeech => "",
            OverlayPhase::Failure | OverlayPhase::Hidden => self.visible_label,
        }
    }

    pub const fn is_visible(self) -> bool { !matches!(self.phase, OverlayPhase::Hidden) }

}

/// The text glyph shown in the capsule's glyph slot. Graphics-first phases
/// (Recording's live bars, Processing's light sweep, NoSpeech's amber floor)
/// carry no glyph; only Success ("✓") and Failure ("⚠") keep one.
pub const fn phase_glyph(phase: OverlayPhase) -> &'static str {
    match phase {
        OverlayPhase::Failure => "⚠",
        OverlayPhase::Success => "✓",
        OverlayPhase::Recording
        | OverlayPhase::Processing
        | OverlayPhase::NoSpeech
        | OverlayPhase::Hidden => "",
    }
}

const TERMINAL_DISPLAY: Duration = Duration::from_secs(2);

#[derive(Debug, Default)]
pub struct PresentationController {
    /// The `(instance, id)` of the last terminal event shown. Scoping by daemon
    /// instance is what lets a restarted daemon reuse id 1 without the observer
    /// mistaking it for the already-displayed event and suppressing the flash.
    displayed_event: Option<(u64, u64)>,
    terminal_until: Option<Instant>,
    /// Deadline for the daemon-unavailable flash, deliberately separate from
    /// `terminal_until`: an unreachable blip must never extend or consume a
    /// terminal event's display window, and vice versa.
    unavailable_until: Option<Instant>,
    /// Whether the last poll observed the daemon as unreachable. Edge-triggering
    /// the daemon-unavailable flash off this flag keeps a persistently-down
    /// daemon from re-arming the capsule on every level-triggered poll.
    unreachable: bool,
}

impl PresentationController {
    pub fn observe(&mut self, response: &Response, now: Instant) -> OverlayView {
        // A successful reachable observation clears the unreachable edge so a
        // LATER reachable->unreachable transition flashes the capsule again,
        // and drops the unavailable deadline so it cannot leak into a terminal
        // event's window.
        self.unreachable = false;
        self.unavailable_until = None;
        // Any live in-progress state (Recording or Processing) is driven straight
        // from status and supersedes the previous terminal feedback window. The
        // retained observer event stays attached to every OverlayStatus response,
        // so it must be ignored while the daemon is not Idle.
        if matches!(
            response.state,
            Some(DaemonState::Recording) | Some(DaemonState::Processing)
        ) {
            self.terminal_until = None;
            return OverlayView::from_response(response);
        }
        if let Some(event) = response.overlay_event.as_ref()
            && self.displayed_event != Some((event.instance, event.id))
        {
            self.displayed_event = Some((event.instance, event.id));
            self.terminal_until = Some(now + TERMINAL_DISPLAY);
            return OverlayView::from_terminal_event(event);
        }
        if self.terminal_until.is_some_and(|until| now < until) {
            return response.overlay_event.as_ref()
                .map(OverlayView::from_terminal_event).unwrap_or(OverlayView::HIDDEN);
        }
        self.terminal_until = None;
        OverlayView::HIDDEN
    }

    /// Routes an unreachable daemon through the same terminal-cap mechanism as
    /// every other terminal event. The reachable->unreachable transition (edge)
    /// flashes the daemon-unavailable capsule for `TERMINAL_DISPLAY`, then hides
    /// while the daemon stays down. The overlay coming up against an
    /// already-down daemon is itself a transition, so it flashes once. A
    /// successful `observe` re-arms the edge for a later drop.
    pub fn observe_unreachable(&mut self, now: Instant) -> OverlayView {
        if !self.unreachable {
            self.unreachable = true;
            self.unavailable_until = Some(now + TERMINAL_DISPLAY);
            return OverlayView::daemon_unavailable();
        }
        if self.unavailable_until.is_some_and(|until| now < until) {
            return OverlayView::daemon_unavailable();
        }
        self.unavailable_until = None;
        OverlayView::HIDDEN
    }
}

/// The pure "WHEN to re-present" decision for the fallback (non-layer-shell)
/// window, kept out of the GTK adapter so it is unit-testable.
///
/// A layer-shell surface is kept above by the compositor, but Wayland gives a
/// plain regular toplevel neither keep-above nor a programmatic raise. The
/// overlay therefore re-`present()`s the window on each transition INTO a new
/// visible phase so it resurfaces above whatever occluded it — and *only* on
/// that edge, never on every 200 ms level-triggered redisplay (e.g. Recording
/// activity ticks), which would fight the user's focus. Resurfacing is keyed on
/// the RENDERED phase because a re-present is exactly what a newly-visible
/// capsule needs, unreachable-blip capsule included.
#[derive(Debug, Default)]
pub struct PresentationTracker {
    last_phase: OverlayPhase,
}

impl PresentationTracker {
    /// Returns true exactly once per transition INTO a visible rendered phase.
    /// A repeat of the same phase, or any transition to Hidden, yields false.
    pub fn observe(&mut self, view: OverlayView) -> bool {
        let resurface = view.phase != self.last_phase && view.is_visible();
        self.last_phase = view.phase;
        resurface
    }
}

/// The successfully-observed daemon signal that drives the Recording-start
/// notification latch. Deliberately DISTINCT from the rendered phase: a failed
/// status read renders a "Daemon unavailable" capsule, but that is not a
/// reachable observation of the daemon's state, so it must leave the latch
/// untouched. Deriving the notify edge from rendered phases instead would let a
/// transient read failure mid-Recording refire the notification when Recording
/// resumes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservedSignal {
    /// `read_status` failed this tick — the daemon's state was not observed.
    Unreachable,
    /// The daemon was reached and rendered to this phase (Recording, Processing,
    /// Idle→Hidden, or a terminal Success/Failure event).
    Reachable(OverlayPhase),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LevelPollAction {
    Arm,
    Disarm,
    Keep,
}

#[derive(Debug, Default)]
pub struct LevelPollLatch {
    armed: bool,
    unreachable_once: bool,
}

impl LevelPollLatch {
    pub fn observe(&mut self, signal: ObservedSignal) -> LevelPollAction {
        match signal {
            ObservedSignal::Unreachable if self.armed && !self.unreachable_once => {
                self.unreachable_once = true;
                LevelPollAction::Keep
            }
            ObservedSignal::Unreachable if self.armed => {
                self.armed = false;
                self.unreachable_once = false;
                LevelPollAction::Disarm
            }
            ObservedSignal::Unreachable => LevelPollAction::Keep,
            ObservedSignal::Reachable(OverlayPhase::Recording) if !self.armed => {
                self.armed = true;
                self.unreachable_once = false;
                LevelPollAction::Arm
            }
            ObservedSignal::Reachable(OverlayPhase::Recording) => {
                self.unreachable_once = false;
                LevelPollAction::Keep
            }
            ObservedSignal::Reachable(_) if self.armed => {
                self.armed = false;
                self.unreachable_once = false;
                LevelPollAction::Disarm
            }
            ObservedSignal::Reachable(_) => {
                self.unreachable_once = false;
                LevelPollAction::Keep
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct BarSmoother {
    levels: [f32; 20],
}

impl BarSmoother {
    pub fn observe(&mut self, bands: [u8; 20]) -> [u8; 20] {
        for (current, target) in self.levels.iter_mut().zip(bands) {
            let coefficient = if target as f32 > *current { 0.65 } else { 0.16 };
            *current += coefficient * (target as f32 - *current);
        }
        self.current()
    }

    pub fn observe_all(&mut self, frames: impl IntoIterator<Item = [u8; 20]>) -> [u8; 20] {
        for frame in frames {
            self.observe(frame);
        }
        self.current()
    }

    pub fn observe_failure(&mut self) -> [u8; 20] {
        self.observe([0; 20])
    }

    pub fn reset(&mut self) -> [u8; 20] {
        self.levels = [0.0; 20];
        self.current()
    }

    pub fn current(&self) -> [u8; 20] {
        std::array::from_fn(|index| self.levels[index].round().clamp(0.0, 255.0) as u8)
    }
}

// ---- Waveform drawing math (pure; the cairo adapter in the bin consumes it) ----

/// Number of bars actually drawn in the meter row. The IPC contract carries
/// exactly 20 frequency bands (`[u8; 20]`, see `LevelFrame`); the extra visual
/// bars are synthesised in the draw code by linear interpolation between those
/// 20 bands (`interpolate_bands`) so the waveform reads as denser and more
/// reactive without changing the wire format.
pub const VISUAL_BAR_COUNT: usize = 44;

/// Spec floor for the edge-falloff ramp: outermost bars sit at ~45% opacity.
const EDGE_FALLOFF_MIN: f64 = 0.45;
/// One full left→right pass of the Processing light sweep.
const SWEEP_PERIOD_SECS: f64 = 1.2;
/// Sweep bump width in bar units (gaussian sigma). Chosen relative to the
/// 44-bar visual row: at 2.2× the density of the old 20-bar row (44/20), the
/// sigma is scaled by the same factor (2.0 × 44/20 = 4.4) so the light-sweep
/// bump keeps the same VISUAL width on screen.
const SWEEP_SIGMA_BARS: f64 = 4.4;
/// Resting-bar brightness away from the sweep bump, and the uniform
/// reduced-motion brightness. Chosen so the sweep reads as light passing
/// through visible bars, not bars blinking on from black.
const SWEEP_BASE_BRIGHTNESS: f64 = 0.35;
const SWEEP_REDUCED_MOTION_BRIGHTNESS: f64 = 0.6;

/// Resample the 20 IPC bands up (or down) to `count` visual levels by linear
/// interpolation, returning levels in `0.0..=255.0`. For visual bar `i` the
/// sample position in band space is `(i + 0.5) / count * 20 - 0.5`; the two
/// neighbouring band indices are clamped to `0..=19` and lerped. `count == 20`
/// returns the bands unchanged (as f64).
pub fn interpolate_bands(bands: &[u8; 20], count: usize) -> Vec<f64> {
    if count == 20 {
        return bands.iter().map(|&b| f64::from(b)).collect();
    }
    (0..count)
        .map(|i| {
            let pos = (i as f64 + 0.5) / count as f64 * 20.0 - 0.5;
            let lo = pos.floor().clamp(0.0, 19.0) as usize;
            let hi = pos.ceil().clamp(0.0, 19.0) as usize;
            let lo_v = f64::from(bands[lo]);
            let hi_v = f64::from(bands[hi]);
            let frac = (pos - lo as f64).clamp(0.0, 1.0);
            lo_v + (hi_v - lo_v) * frac
        })
        .collect()
}

/// Per-bar opacity: a half-sine ramp from `EDGE_FALLOFF_MIN` at the row's ends
/// to 1.0 at its center, so the waveform fades out softly instead of stopping.
pub fn edge_falloff_alpha(index: usize, count: usize) -> f64 {
    let position = (index as f64 + 0.5) / count as f64;
    EDGE_FALLOFF_MIN + (1.0 - EDGE_FALLOFF_MIN) * (std::f64::consts::PI * position).sin()
}

/// Silence baseline: 10% of the drawable height, never below the old 1.5px
/// minimum. A dotted resting line reads "listening", a flatline reads "dead".
pub fn resting_floor(drawable_height: f64) -> f64 {
    (0.10 * drawable_height).max(1.5)
}

/// Recording bar height: level 0 rests exactly on the floor, level 255 fills
/// the drawable height, linear in between.
pub fn recording_bar_height(level: u8, drawable_height: f64) -> f64 {
    let floor = resting_floor(drawable_height);
    floor + f64::from(level) / 255.0 * (drawable_height - floor)
}

/// Processing light-sweep brightness for one bar. A gaussian bump travels
/// left→right across the row every `SWEEP_PERIOD_SECS`, entering from before
/// bar 0 and exiting past the last bar so the loop has no visible snap.
/// Reduced motion: uniform raised brightness, no movement.
pub fn sweep_brightness(index: usize, count: usize, elapsed_secs: f64, reduced_motion: bool) -> f64 {
    if reduced_motion {
        return SWEEP_REDUCED_MOTION_BRIGHTNESS;
    }
    let progress = (elapsed_secs.rem_euclid(SWEEP_PERIOD_SECS)) / SWEEP_PERIOD_SECS;
    let travel = count as f64 + 6.0 * SWEEP_SIGMA_BARS;
    let position = progress * travel - 3.0 * SWEEP_SIGMA_BARS;
    let distance = index as f64 + 0.5 - position;
    let bump = (-distance * distance / (2.0 * SWEEP_SIGMA_BARS * SWEEP_SIGMA_BARS)).exp();
    SWEEP_BASE_BRIGHTNESS + (1.0 - SWEEP_BASE_BRIGHTNESS) * bump
}

/// Edge-latch for the fallback path's secondary "Recording started" desktop
/// notification. Pure and adapter-free, mirroring `PresentationTracker`.
///
/// Fires once when a REACHABLE Recording observation begins and stays silent
/// until a reachable non-Recording observation (Idle, Processing, or a terminal
/// event) re-arms it. An `Unreachable` signal leaves the latch untouched, so a
/// transient blip mid-Recording never produces a duplicate notification.
#[derive(Debug, Default)]
pub struct RecordingNotifyLatch {
    latched: bool,
}

impl RecordingNotifyLatch {
    pub fn observe(&mut self, signal: ObservedSignal) -> bool {
        match signal {
            ObservedSignal::Unreachable => false,
            ObservedSignal::Reachable(OverlayPhase::Recording) => {
                let fire = !self.latched;
                self.latched = true;
                fire
            }
            ObservedSignal::Reachable(_) => {
                self.latched = false;
                false
            }
        }
    }
}

/// Edge-latch for the "Didn't catch any speech" desktop notification. Unlike
/// `RecordingNotifyLatch` (fallback-path only), this fires on BOTH windowed
/// paths: the amber capsule shows WHAT happened, the notification explains it,
/// and on layer-shell the capsule alone can be missed at the screen edge.
/// Same blip rule: `Unreachable` leaves the latch untouched.
#[derive(Debug, Default)]
pub struct NoSpeechNotifyLatch {
    latched: bool,
}

impl NoSpeechNotifyLatch {
    pub fn observe(&mut self, signal: ObservedSignal) -> bool {
        match signal {
            ObservedSignal::Unreachable => false,
            ObservedSignal::Reachable(OverlayPhase::NoSpeech) => {
                let fire = !self.latched;
                self.latched = true;
                fire
            }
            ObservedSignal::Reachable(_) => {
                self.latched = false;
                false
            }
        }
    }
}

/// How close a live Recording is to the Recording Deadline. Ordered: `Final`
/// supersedes `Approaching`, which is what lets one latch hold both stages
/// without a second flag.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum LimitWarning {
    /// One minute of headroom left — amber bars, "start wrapping up".
    Approaching,
    /// Ten seconds left — the red warn border joins the amber bars.
    Final,
}

/// Lead time before the Deadline at which the first warning fires.
pub const APPROACHING_LIMIT_LEAD: Duration = Duration::from_secs(60);
/// Lead time before the Deadline at which the final warning fires.
pub const FINAL_LIMIT_LEAD: Duration = Duration::from_secs(10);

/// The elapsed-time onsets of the two warnings for a given Recording ceiling,
/// as `(approaching, final)`. Purely derived: there is no warning constant to
/// drift out of sync with the ceiling, so moving the ceiling moves both
/// warnings with it. Saturating, so a ceiling shorter than a lead simply means
/// that warning is live from the first tick rather than wrapping.
pub fn limit_warning_onsets(ceiling: Duration) -> (Duration, Duration) {
    (
        ceiling.saturating_sub(APPROACHING_LIMIT_LEAD),
        ceiling.saturating_sub(FINAL_LIMIT_LEAD),
    )
}

/// The warning stage implied by the headroom the daemon reports. `None`
/// remaining means the daemon is not Recording, which is never a warning.
///
/// Headroom, not elapsed time, is what crosses the wire: the ceiling lives with
/// the enforcer, so the subtraction happens once, there, and the presentation
/// layer never holds a copy of the ceiling to fall out of step with.
pub fn limit_warning_for_remaining(remaining: Option<Duration>) -> Option<LimitWarning> {
    let remaining = remaining?;
    if remaining <= FINAL_LIMIT_LEAD {
        Some(LimitWarning::Final)
    } else if remaining <= APPROACHING_LIMIT_LEAD {
        Some(LimitWarning::Approaching)
    } else {
        None
    }
}

/// The warning stage at `elapsed` against `ceiling`. The same decision as
/// [`limit_warning_for_remaining`], expressed in the terms the ceiling is
/// written in so a test can pin the derivation directly.
pub fn limit_warning_at(elapsed: Duration, ceiling: Duration) -> Option<LimitWarning> {
    limit_warning_for_remaining(Some(ceiling.saturating_sub(elapsed)))
}

/// The headroom the daemon reported on this status reply, if it was Recording.
/// A daemon too old to know the field simply reports nothing, which reads as
/// "no warning" rather than as a warning at zero.
pub fn recording_remaining(response: &Response) -> Option<Duration> {
    response.recording_remaining_ms.map(Duration::from_millis)
}

/// The warning stage implied by a status reply.
pub fn limit_warning_from_response(response: &Response) -> Option<LimitWarning> {
    limit_warning_for_remaining(recording_remaining(response))
}

/// Which Recording this reply is about, taken from the correlation ID that
/// already joins every event of one Recording. The Overlay needs it because
/// phase alone cannot tell two back-to-back Recordings apart: at a 200 ms poll
/// a stop and a restart can both land inside one gap, and a daemon restart
/// looks the same.
pub fn recording_identity(response: &Response) -> Option<&str> {
    response
        .evidence
        .as_ref()
        .map(|evidence| evidence.correlation_id.as_str())
        .filter(|identity| !identity.is_empty())
}

/// The Recording meter's bar colour, amber once the limit is approaching. Both
/// warning stages keep the amber bars; the final stage only adds the border.
pub const RECORDING_BAR_RGB: (f64, f64, f64) = (0.949, 0.949, 0.949);
/// The same amber the NoSpeech capsule uses (#FFB454) — one warm accent, not a
/// second palette.
pub const LIMIT_WARNING_BAR_RGB: (f64, f64, f64) = (1.0, 0.706, 0.329);

/// Bar colour for a Recording at a given warning stage.
pub fn recording_bar_rgb(warning: Option<LimitWarning>) -> (f64, f64, f64) {
    match warning {
        Some(_) => LIMIT_WARNING_BAR_RGB,
        None => RECORDING_BAR_RGB,
    }
}

/// The CSS class carrying the red warn border.
pub const LIMIT_WARNING_CLASS: &str = "limitwarn";

/// The capsule's warn-border class for a warning stage. Only the final stage
/// earns the border: an amber-bar minute followed by a border at ten seconds is
/// an escalation, whereas bordering both stages would flatten them into one.
pub fn limit_warning_class(warning: Option<LimitWarning>) -> Option<&'static str> {
    matches!(warning, Some(LimitWarning::Final)).then_some(LIMIT_WARNING_CLASS)
}

/// The notification body for a warning stage at its nominal lead — the exact
/// approved wording, used whenever it is actually true.
pub const fn limit_warning_body(warning: LimitWarning) -> &'static str {
    match warning {
        LimitWarning::Approaching => "Approaching the recording limit — about a minute left",
        LimitWarning::Final => "Recording stops in 10 seconds",
    }
}

/// The lead this stage's approved wording describes.
pub const fn limit_warning_lead(warning: LimitWarning) -> Duration {
    match warning {
        LimitWarning::Approaching => APPROACHING_LIMIT_LEAD,
        LimitWarning::Final => FINAL_LIMIT_LEAD,
    }
}

/// What to actually say for a warning stage, given the headroom really left.
///
/// `VOISU_RECORDING_DEADLINE_MS` is operator-reachable, so a ceiling can be
/// shorter than a lead: a 30 s ceiling would announce "about a minute left" at
/// the very first tick, and a 5 s ceiling would promise ten seconds it does not
/// have. A warning the user can time against and catch lying is worse than no
/// warning. The approved wording is kept verbatim whenever the headroom really
/// is the lead it describes, and replaced by the true figure when it is not.
///
/// `None` means say nothing: at zero headroom the stop is already in flight,
/// and counting down to a moment that has passed helps nobody.
pub fn limit_notification_body(warning: LimitWarning, remaining: Duration) -> Option<String> {
    let seconds = remaining.as_secs_f64().round() as u64;
    if seconds == 0 {
        return None;
    }
    if seconds == limit_warning_lead(warning).as_secs() {
        return Some(limit_warning_body(warning).to_owned());
    }
    let unit = if seconds == 1 { "second" } else { "seconds" };
    Some(match warning {
        LimitWarning::Approaching => {
            format!("Approaching the recording limit — about {seconds} {unit} left")
        }
        LimitWarning::Final => format!("Recording stops in {seconds} {unit}"),
    })
}

/// Edge-latch for the approaching-limit notifications, mirroring
/// [`NoSpeechNotifyLatch`]. The overlay redraws several times a second, so a
/// bare threshold test would emit one notification per frame; the latch fires
/// each stage at most once per Recording and only ever escalates.
///
/// Same blip rule as the other latches: `Unreachable` leaves the latch
/// untouched, so a transient status-read failure mid-Recording cannot replay a
/// warning. Any reachable non-Recording observation clears it, which is what
/// stops a short Recording from leaving residue for the next one.
#[derive(Debug, Default)]
pub struct LimitWarningLatch {
    fired: Option<LimitWarning>,
    /// `Some(prior)` while this tick's announcement is still outstanding —
    /// handed out but not yet accepted by a sink. Cleared at the start of every
    /// observation, so an announcement can only be rolled back within the tick
    /// that produced it. Without it the latch commits a warning that was never
    /// delivered and the user hears nothing for the rest of the Recording.
    uncommitted: Option<Option<LimitWarning>>,
    /// The Recording the fired stages belong to. Phase transitions alone are
    /// not a reliable boundary between Recordings, so the latch carries the
    /// identity and clears the moment it changes.
    identity: Option<String>,
}

impl LimitWarningLatch {
    /// Returns the warning to announce this tick, or `None`. A stage is
    /// announced only when it is strictly beyond everything already announced,
    /// so repeated ticks inside the same window stay silent and a tick that
    /// jumps clean over the first window announces only the final warning
    /// rather than backfilling a stale "about a minute left".
    pub fn observe(
        &mut self,
        signal: ObservedSignal,
        identity: Option<&str>,
        warning: Option<LimitWarning>,
    ) -> Option<LimitWarning> {
        // A new observation supersedes the last one: whatever was outstanding
        // is now this tick's problem, not the previous tick's.
        self.uncommitted = None;
        match signal {
            // Deliberately no identity check here: an unreachable poll observes
            // nothing, so it must not look like a new Recording.
            ObservedSignal::Unreachable => None,
            ObservedSignal::Reachable(OverlayPhase::Recording) => {
                // A different Recording starts from silence even if the
                // observer never saw a non-Recording phase between the two.
                if self.identity.as_deref() != identity {
                    self.identity = identity.map(str::to_owned);
                    self.fired = None;
                }
                if warning > self.fired {
                    self.uncommitted = Some(self.fired);
                    self.fired = warning;
                    warning
                } else {
                    None
                }
            }
            ObservedSignal::Reachable(_) => {
                self.fired = None;
                self.identity = None;
                None
            }
        }
    }

    /// Hand back the stage the last [`observe`](Self::observe) handed out,
    /// because the sink refused it. The next tick re-selects a warning, so the
    /// user still hears one — a silently dropped announcement would otherwise
    /// be silence for the rest of the Recording.
    ///
    /// Escalate-only survives the rollback: a returned `Approaching` that has
    /// since become `Final` is re-selected as `Final` alone, never as both.
    /// Calling this without an outstanding announcement, or twice for the same
    /// one, changes nothing.
    pub fn rollback(&mut self) {
        if let Some(prior) = self.uncommitted.take() {
            self.fired = prior;
        }
    }

    /// The highest stage already announced for the current Recording. Exposed
    /// so a test can prove no residue survives into the next Recording.
    pub fn fired(&self) -> Option<LimitWarning> {
        self.fired
    }
}

/// The one thing the desktop-notification rung may say on a single tick.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RungNotification {
    /// The approaching-limit warning; its body depends on the real headroom.
    Limit,
    /// The capsule's own label — the no-speech explanation, or a transition
    /// into a visible phase.
    Label(&'static str),
}

/// The single notification the desktop-notification rung sends this tick.
///
/// Unlike the windowed paths, this rung has one bubble: its channel is depth-1
/// with a non-blocking send, and it reuses one replaces_id, so a second send on
/// the same tick is either dropped or paints over the first. Both can genuinely
/// come due together — the first observation of a Recording that is ALREADY
/// inside a warning window is a phase transition and a warning at once — and
/// the latch has already consumed the stage, so a lost warning never returns.
/// The warning therefore wins: it is time-critical and the transition is not.
pub fn notification_rung_choice(
    view: OverlayView,
    previous_phase: OverlayPhase,
    limit_pending: bool,
    fire_no_speech: bool,
) -> Option<RungNotification> {
    if limit_pending {
        return Some(RungNotification::Limit);
    }
    if view.phase == OverlayPhase::NoSpeech {
        return fire_no_speech.then_some(RungNotification::Label(view.visible_label));
    }
    // Fire only on a PHASE transition into a visible phase. Comparing the whole
    // view would re-fire on every meter/activity tick within one Recording.
    (view.is_visible() && previous_phase != view.phase)
        .then_some(RungNotification::Label(view.visible_label))
}

/// The outcome of one fallback-path poll tick, decided purely so the adapter's
/// side effects (`window.present()`, `send_notification`) stay a thin match.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TickAction {
    /// Stop driving the window this tick and break the poll loop.
    Break,
    /// Keep polling; `resurface`/`notify`/`notify_no_speech`/`notify_limit`
    /// say which side effects to run.
    Continue {
        resurface: bool,
        notify: bool,
        notify_no_speech: bool,
        notify_limit: Option<LimitWarning>,
    },
}

/// Pure decision for a single poll tick, owning the ordering the adapter relied
/// on implicitly. Crucially, a surface handoff detected AFTER `render_surface`
/// (`switched_after_render`) yields `Break` BEFORE the tracker or latch observe
/// the tick — so a retired (handed-off) window is never re-presented and no
/// duplicate notification is sent on the same tick. Keeping this ordering pure
/// lets a test pin it; a future refactor that drops the guard fails the test.
pub fn poll_tick(
    switched_after_render: bool,
    is_fallback: bool,
    view: OverlayView,
    signal: ObservedSignal,
    identity: Option<&str>,
    warning: Option<LimitWarning>,
    tracker: &mut PresentationTracker,
    notify_latch: &mut RecordingNotifyLatch,
    no_speech_latch: &mut NoSpeechNotifyLatch,
    limit_latch: &mut LimitWarningLatch,
) -> TickAction {
    if switched_after_render {
        return TickAction::Break;
    }
    let notify_no_speech = no_speech_latch.observe(signal);
    // Fires on BOTH windowed paths, for the same reason NoSpeech does: on
    // layer-shell the capsule sits at the screen edge and the colour change
    // alone can be missed, and this warning has a deadline attached.
    let notify_limit = limit_latch.observe(signal, identity, warning);
    if !is_fallback {
        return TickAction::Continue {
            resurface: false,
            notify: false,
            notify_no_speech,
            notify_limit,
        };
    }
    let resurface = tracker.observe(view);
    let notify = notify_latch.observe(signal);
    TickAction::Continue { resurface, notify, notify_no_speech, notify_limit }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feedback::{select_feedback_backend, FeedbackBackend, FeedbackCapabilities, SessionKind};
    use voisu_core::{DaemonState, OverlayEvent, OverlayOutcome, Response, VersionEnvelope};

    #[test]
    fn red_bar_smoothing_attacks_quickly_and_releases_monotonically() {
        let mut smoother = BarSmoother::default();
        let first = smoother.observe([200; 20]);
        let second = smoother.observe([200; 20]);
        assert!(first[0] >= 100 && second[0] > first[0]);
        let release_one = smoother.observe([0; 20]);
        let release_two = smoother.observe([0; 20]);
        assert!(release_one[0] < second[0] && release_two[0] < release_one[0]);
    }

    #[test]
    fn red_coalesced_level_frames_preserve_an_intermediate_peak() {
        let mut coalesced = BarSmoother::default();
        let result = coalesced.observe_all([[20; 20], [240; 20], [20; 20]]);
        let mut last_only = BarSmoother::default();
        let last = last_only.observe([20; 20]);
        assert!(result[0] > last[0]);
    }

    #[test]
    fn red_level_poll_timer_uses_observed_recording_edges() {
        let mut latch = LevelPollLatch::default();
        assert_eq!(latch.observe(ObservedSignal::Reachable(OverlayPhase::Recording)), LevelPollAction::Arm);
        assert_eq!(latch.observe(ObservedSignal::Reachable(OverlayPhase::Recording)), LevelPollAction::Keep);
        assert_eq!(latch.observe(ObservedSignal::Unreachable), LevelPollAction::Keep);
        assert_eq!(
            latch.observe(ObservedSignal::Reachable(OverlayPhase::Recording)),
            LevelPollAction::Keep
        );
        assert_eq!(latch.observe(ObservedSignal::Reachable(OverlayPhase::Processing)), LevelPollAction::Disarm);
        assert_eq!(latch.observe(ObservedSignal::Reachable(OverlayPhase::Hidden)), LevelPollAction::Keep);
    }

    #[test]
    fn red_persistent_unreachability_disarms_the_fast_poll_without_a_clock() {
        let mut latch = LevelPollLatch::default();
        assert_eq!(
            latch.observe(ObservedSignal::Reachable(OverlayPhase::Recording)),
            LevelPollAction::Arm
        );
        assert_eq!(latch.observe(ObservedSignal::Unreachable), LevelPollAction::Keep);
        assert_eq!(
            latch.observe(ObservedSignal::Unreachable),
            LevelPollAction::Disarm
        );
    }

    #[test]
    fn red_non_recording_phases_keep_their_pre_waveform_glyphs() {
        // Recording and Processing are both graphics-only (no glyph): the
        // live bar meter carries Recording, and the light sweep carries
        // Processing with no text or glyph (spec 2026-07-23, graphics-first
        // redesign). Failure and Success keep their glyphs.
        assert_eq!(phase_glyph(OverlayPhase::Processing), "");
        assert_eq!(phase_glyph(OverlayPhase::Failure), "⚠");
        assert_eq!(phase_glyph(OverlayPhase::Recording), "");
        assert_eq!(phase_glyph(OverlayPhase::Success), "✓");
        assert_eq!(phase_glyph(OverlayPhase::Hidden), "");
    }

    #[test]
    fn red_a_failed_level_poll_only_decays_bars() {
        let now = Instant::now();
        let mut controller = PresentationController::default();
        let response = overlay_status(DaemonState::Recording, None);
        assert_eq!(controller.observe(&response, now).phase, OverlayPhase::Recording);
        let mut smoother = BarSmoother::default();
        smoother.observe([200; 20]);
        assert!(smoother.observe_failure()[0] < 200);
        assert_eq!(controller.observe(&response, now).phase, OverlayPhase::Recording);
    }

    fn event(id: u64, outcome: OverlayOutcome) -> OverlayEvent {
        event_from(0, id, outcome)
    }

    fn event_from(instance: u64, id: u64, outcome: OverlayOutcome) -> OverlayEvent {
        OverlayEvent { id, instance, outcome, message: "exact public outcome".into() }
    }

    /// Mirrors a real `OverlayStatus` reply: the observer path always attaches
    /// the retained terminal event, whatever the current daemon state is.
    fn overlay_status(state: DaemonState, retained: Option<OverlayEvent>) -> Response {
        let mut response = Response::success(state, state.cli_label());
        response.overlay_event = retained;
        response
    }

    #[test]
    fn startup_is_hidden_at_idle_and_an_immediate_recording_is_visible_without_a_grace_window() {
        // Round-2 finding 2: the window must stay hidden at Idle (no styled
        // empty-capsule flash) and polling must start immediately so an early
        // Recording is never missed. The pure decision the adapter honors is
        // tested here; only a live compositor can prove the absence of the
        // startup visual flash, which the adapter now guarantees by never
        // calling `present()` at startup.
        let now = Instant::now();
        // Before any status arrives the view is HIDDEN — the window stays down.
        assert_eq!(OverlayView::HIDDEN.phase, OverlayPhase::Hidden);
        assert!(!OverlayView::HIDDEN.is_visible());
        // An Idle daemon keeps the window hidden: no startup flash.
        let mut at_idle = PresentationController::default();
        let idle = at_idle.observe(&overlay_status(DaemonState::Idle, None), now);
        assert_eq!(idle.phase, OverlayPhase::Hidden);
        assert!(!idle.is_visible());
        // The very first observed status can be Recording; it is immediately
        // visible with no grace window, so immediate polling shows it at once.
        let mut fresh = PresentationController::default();
        let recording = fresh.observe(&overlay_status(DaemonState::Recording, None), now);
        assert_eq!(recording.phase, OverlayPhase::Recording);
        assert!(recording.is_visible());
    }

    #[test]
    fn public_observer_response_is_typed_and_terminal_events_are_displayed_once() {
        let terminal = overlay_status(DaemonState::Idle, Some(event(7, OverlayOutcome::DeliveryFailure)));
        let mut controller = PresentationController::default();
        let now = Instant::now();
        assert_eq!(controller.observe(&terminal, now).phase, OverlayPhase::Failure);
        assert_eq!(controller.observe(&terminal, now).phase, OverlayPhase::Failure);
        assert_eq!(controller.observe(&terminal, now + TERMINAL_DISPLAY).phase, OverlayPhase::Hidden);
    }

    #[test]
    fn next_recording_clears_terminal_feedback_and_is_not_lifecycle_coupled() {
        // The daemon retains the last terminal event on every OverlayStatus
        // reply, so the next-Recording sequence must still carry it — unlike a
        // response with no field, this proves the controller dedups by id and
        // respects expiry rather than trivially going hidden.
        let mut controller = PresentationController::default();
        let now = Instant::now();
        let stale = event(1, OverlayOutcome::QualityFailure);
        let terminal = overlay_status(DaemonState::Idle, Some(stale.clone()));
        assert_eq!(controller.observe(&terminal, now).phase, OverlayPhase::NoSpeech);
        // The next Recording (with the stale event still retained) overrides the
        // terminal feedback and is driven live from status.
        let recording = overlay_status(DaemonState::Recording, Some(stale.clone()));
        assert_eq!(controller.observe(&recording, now).phase, OverlayPhase::Recording);
        // Returning to Idle with the same already-shown, expired event stays hidden.
        let idle = overlay_status(DaemonState::Idle, Some(stale));
        assert_eq!(controller.observe(&idle, now).phase, OverlayPhase::Hidden);
    }

    #[test]
    fn processing_is_shown_live_from_status_over_a_retained_terminal_event() {
        // The retained observer event stays attached during Processing. A
        // status-driven live state must win over that stale terminal feedback,
        // whether or not the event was already displayed.
        let mut controller = PresentationController::default();
        let now = Instant::now();
        let delivered = event(5, OverlayOutcome::Delivered);
        assert_eq!(
            controller.observe(&overlay_status(DaemonState::Idle, Some(delivered.clone())), now).phase,
            OverlayPhase::Success,
        );
        assert_eq!(
            controller.observe(&overlay_status(DaemonState::Recording, Some(delivered.clone())), now).phase,
            OverlayPhase::Recording,
        );
        // Already-displayed retained event + Processing must render Processing,
        // not the stale terminal event and not hidden.
        assert_eq!(
            controller.observe(&overlay_status(DaemonState::Processing, Some(delivered)), now).phase,
            OverlayPhase::Processing,
        );
        // A fresh observer that first sees Processing with an undisplayed
        // retained event still renders Processing, never the terminal event.
        assert_eq!(
            PresentationController::default()
                .observe(&overlay_status(DaemonState::Processing, Some(event(9, OverlayOutcome::DeliveryFailure))), now)
                .phase,
            OverlayPhase::Processing,
        );
    }

    #[test]
    fn the_exact_terminal_id_reused_by_a_restarted_daemon_is_still_shown() {
        // A restarted daemon resets its id counter to 1, so its first terminal
        // event reuses the EXACT id (1) the observer just displayed. Identity is
        // scoped by (instance, id), so the new instance disambiguates it; keying
        // on the bare id would suppress this flash entirely.
        let instance_a = 0xAAAA_0001;
        let instance_b = 0xBBBB_0002;
        let mut controller = PresentationController::default();
        let t0 = Instant::now();
        assert_eq!(
            controller
                .observe(&overlay_status(DaemonState::Idle, Some(event_from(instance_a, 1, OverlayOutcome::Delivered))), t0)
                .phase,
            OverlayPhase::Success,
        );
        // The terminal window expires and the same retained event stays hidden.
        let t1 = t0 + TERMINAL_DISPLAY + Duration::from_millis(1);
        assert_eq!(
            controller
                .observe(&overlay_status(DaemonState::Idle, Some(event_from(instance_a, 1, OverlayOutcome::Delivered))), t1)
                .phase,
            OverlayPhase::Hidden,
        );
        // Daemon restarts: new instance, id counter reset to 1 (exact collision).
        assert_eq!(
            controller
                .observe(&overlay_status(DaemonState::Idle, Some(event_from(instance_b, 1, OverlayOutcome::DeliveryFailure))), t1)
                .phase,
            OverlayPhase::Failure,
        );
    }

    #[test]
    fn an_unreachable_blip_near_expiry_cannot_extend_a_terminal_events_window() {
        // Review finding on the shared deadline: a terminal event shown at t0,
        // a daemon drop just before its 2s window expires, then recovery must
        // NOT redisplay the retained event against the unavailable deadline —
        // that would stretch a nominal 2-second capsule to nearly 4 seconds.
        let mut controller = PresentationController::default();
        let t0 = Instant::now();
        let delivered = event(3, OverlayOutcome::Delivered);
        let terminal = overlay_status(DaemonState::Idle, Some(delivered));
        assert_eq!(controller.observe(&terminal, t0).phase, OverlayPhase::Success);
        // Daemon drops just before the terminal window expires: the
        // unavailable capsule flashes on its own deadline.
        let near_expiry = t0 + TERMINAL_DISPLAY - Duration::from_millis(100);
        assert_eq!(
            controller.observe_unreachable(near_expiry).phase,
            OverlayPhase::Failure,
        );
        // Daemon recovers after the terminal window elapsed: the retained
        // event must stay hidden, not ride the unavailable deadline.
        let after_terminal_window = t0 + TERMINAL_DISPLAY + Duration::from_millis(100);
        assert_eq!(
            controller.observe(&terminal, after_terminal_window).phase,
            OverlayPhase::Hidden,
        );
        // Symmetric containment: a terminal window survives an unreachable
        // blip unchanged — shown for its remainder, hidden at its own expiry.
        let mut symmetric = PresentationController::default();
        let fresh = overlay_status(DaemonState::Idle, Some(event(4, OverlayOutcome::Delivered)));
        assert_eq!(symmetric.observe(&fresh, t0).phase, OverlayPhase::Success);
        symmetric.observe_unreachable(t0 + Duration::from_millis(500));
        assert_eq!(
            symmetric.observe(&fresh, t0 + Duration::from_millis(600)).phase,
            OverlayPhase::Success,
        );
        assert_eq!(
            symmetric.observe(&fresh, t0 + TERMINAL_DISPLAY).phase,
            OverlayPhase::Hidden,
        );
    }

    #[test]
    fn a_daemon_unreachable_transition_flashes_the_daemon_unavailable_capsule() {
        // Edge-triggered: the reachable->unreachable transition shows the
        // daemon-unavailable Failure view, with the exact label strings users
        // and tests rely on.
        let mut controller = PresentationController::default();
        let now = Instant::now();
        let view = controller.observe_unreachable(now);
        assert_eq!(view.phase, OverlayPhase::Failure);
        assert!(view.is_visible());
        assert_eq!(view.visible_label, "Daemon unavailable");
        assert_eq!(
            view.accessible_label,
            "Daemon unavailable; the optional Overlay cannot reach voisu-daemon",
        );
    }

    #[test]
    fn a_persistent_unreachable_daemon_hides_after_the_terminal_cap() {
        // The daemon-unavailable capsule obeys the same TERMINAL_DISPLAY cap as
        // every other terminal event: it flashes, then hides while the daemon
        // stays down instead of pinning on screen forever.
        let mut controller = PresentationController::default();
        let now = Instant::now();
        assert_eq!(controller.observe_unreachable(now).phase, OverlayPhase::Failure);
        // Still within the window: the capsule remains up.
        assert_eq!(
            controller.observe_unreachable(now + Duration::from_millis(500)).phase,
            OverlayPhase::Failure,
        );
        // The window elapses while the daemon is still unreachable: hidden.
        assert_eq!(
            controller.observe_unreachable(now + TERMINAL_DISPLAY).phase,
            OverlayPhase::Hidden,
        );
        // It stays hidden as unreachability persists.
        assert_eq!(
            controller
                .observe_unreachable(now + TERMINAL_DISPLAY + Duration::from_secs(30))
                .phase,
            OverlayPhase::Hidden,
        );
    }

    #[test]
    fn a_reachable_observation_rearms_a_later_unreachable_flash() {
        // A successful observe resets the edge: after the daemon comes back and
        // then drops again, the fresh transition flashes once more.
        let mut controller = PresentationController::default();
        let now = Instant::now();
        assert_eq!(controller.observe_unreachable(now).phase, OverlayPhase::Failure);
        let expired = now + TERMINAL_DISPLAY;
        assert_eq!(controller.observe_unreachable(expired).phase, OverlayPhase::Hidden);
        // Daemon reachable again (idle) clears the unreachable edge.
        assert_eq!(
            controller.observe(&overlay_status(DaemonState::Idle, None), expired).phase,
            OverlayPhase::Hidden,
        );
        // A later reachable->unreachable transition flashes again.
        assert_eq!(
            controller.observe_unreachable(expired + Duration::from_secs(1)).phase,
            OverlayPhase::Failure,
        );
    }

    #[test]
    fn continuous_unreachability_does_not_reflash_after_the_cap() {
        // Level-triggered ticks (every 200 ms) while the daemon stays down must
        // not re-arm the flash; only a reachable->unreachable edge does.
        let mut controller = PresentationController::default();
        let now = Instant::now();
        assert_eq!(controller.observe_unreachable(now).phase, OverlayPhase::Failure);
        assert_eq!(
            controller.observe_unreachable(now + TERMINAL_DISPLAY).phase,
            OverlayPhase::Hidden,
        );
        for tick in 1..20 {
            assert_eq!(
                controller
                    .observe_unreachable(now + TERMINAL_DISPLAY + Duration::from_millis(200 * tick))
                    .phase,
                OverlayPhase::Hidden,
                "unreachable tick {tick} must not re-flash",
            );
        }
    }

    #[test]
    fn a_future_or_unknown_terminal_outcome_degrades_to_a_generic_failure() {
        // A newer daemon may report an outcome variant this client predates. It
        // must deserialize into a safe generic failure, not break the response.
        let response: Response = serde_json::from_str(
            r#"{"version":1,"ok":true,"state":"idle","message":"idle","overlay_event":{"id":9,"outcome":"teleported_transcript","message":"x"}}"#,
        ).unwrap();
        assert_eq!(response.overlay_event.as_ref().unwrap().outcome, OverlayOutcome::Unknown);
        assert_eq!(
            PresentationController::default().observe(&response, Instant::now()).phase,
            OverlayPhase::Failure,
        );
    }

    #[test]
    fn responses_from_a_pre_event_daemon_are_safe_and_have_no_stale_feedback() {
        // New client, old daemon: the observer field is simply absent.
        let response: Response = serde_json::from_str(
            r#"{"version":1,"ok":true,"state":"idle","message":"idle"}"#,
        ).unwrap();
        assert!(response.overlay_event.is_none());
        assert_eq!(PresentationController::default().observe(&response, Instant::now()),
            OverlayView::HIDDEN);
    }

    #[test]
    fn an_older_client_tolerates_the_new_observer_only_field() {
        // Old client, new daemon: a reader that only knows the version envelope
        // still parses a response carrying the added observer payload.
        let envelope: VersionEnvelope = serde_json::from_str(
            r#"{"version":1,"ok":true,"state":"idle","message":"idle","overlay_event":{"id":3,"outcome":"delivered","message":"Delivered"}}"#,
        ).unwrap();
        assert_eq!(envelope.version, 1);
    }

    #[test]
    fn red_layer_shell_is_selected_only_for_an_advertised_wayland_compositor() {
        // RED proof: this contract names the public capability seam before the
        // implementation exists. Removing the runtime Layer Shell probe makes
        // this test fail rather than silently choosing a Layer Shell surface.
        let selection = select_feedback_backend(FeedbackCapabilities {
            session: SessionKind::Wayland,
            display_available: true,
            xwayland_fallback: false,
            layer_shell_supported: true,
        });
        assert_eq!(selection.backend, FeedbackBackend::LayerShell);
        assert_eq!(selection.degradation, None);
    }

    #[test]
    fn red_degraded_cases_keep_a_visible_feedback_path_and_name_the_cause() {
        // RED proof: without the pure fallback selector, X11, unavailable
        // Layer Shell, a missing display, and a failed surface are all either
        // silently ignored or crash in GTK-dependent tests.
        let cases = [
            (
                FeedbackCapabilities { session: SessionKind::X11, display_available: true, xwayland_fallback: false, layer_shell_supported: false },
                FeedbackBackend::DesktopNotification,
                Some(crate::feedback::FeedbackDegradation::X11),
            ),
            (
                FeedbackCapabilities { session: SessionKind::Wayland, display_available: true, xwayland_fallback: false, layer_shell_supported: false },
                FeedbackBackend::DesktopNotification,
                Some(crate::feedback::FeedbackDegradation::LayerShellUnavailable),
            ),
            (
                FeedbackCapabilities { session: SessionKind::Wayland, display_available: false, xwayland_fallback: false, layer_shell_supported: true },
                FeedbackBackend::JournalLog,
                Some(crate::feedback::FeedbackDegradation::MissingDisplay),
            ),
        ];
        for (capabilities, backend, degradation) in cases {
            let selection = select_feedback_backend(capabilities);
            assert_eq!((selection.backend, selection.degradation), (backend, degradation));
        }
        let surface_failure = crate::feedback::after_surface_creation(
            select_feedback_backend(FeedbackCapabilities { session: SessionKind::Wayland, display_available: true, xwayland_fallback: false, layer_shell_supported: true }),
            false,
        );
        assert_eq!(surface_failure.backend, FeedbackBackend::DesktopNotification);
        assert_eq!(surface_failure.degradation, Some(crate::feedback::FeedbackDegradation::SurfaceCreationFailure));
    }

    #[test]
    fn red_resurface_fires_once_per_transition_into_a_visible_phase() {
        // RED proof: Wayland denies a regular toplevel keep-above, so the
        // fallback window must be re-presented on each transition INTO a visible
        // phase — and only then. Without the pure `PresentationTracker` edge, the
        // adapter would either never resurface a buried capsule or spam
        // `present()` on every 200 ms level-triggered tick, stealing focus.
        let recording = OverlayView::from_response(&overlay_status(DaemonState::Recording, None));
        let processing = OverlayView::from_response(&overlay_status(DaemonState::Processing, None));
        let mut tracker = PresentationTracker::default();
        // hidden -> Recording: a new visible phase, so resurface once.
        assert!(tracker.observe(recording));
        // Recording -> Recording (a level-triggered redisplay) must NOT re-present.
        assert!(!tracker.observe(recording));
        // Recording -> Processing: a new visible phase, resurface again.
        assert!(tracker.observe(processing));
        // Processing -> Hidden is not a visible phase: never resurface.
        assert!(!tracker.observe(OverlayView::HIDDEN));
        // Hidden -> Hidden never resurfaces.
        assert!(!tracker.observe(OverlayView::HIDDEN));
        // Hidden -> Recording again is a fresh transition: resurface once more.
        assert!(tracker.observe(recording));
    }

    #[test]
    fn red_recording_start_notifies_once_until_a_reachable_reset() {
        // RED proof: the fallback path fires the Recording notification only when
        // a REACHABLE Recording observation begins, and not again while Recording
        // persists across activity ticks.
        let mut latch = RecordingNotifyLatch::default();
        assert!(latch.observe(ObservedSignal::Reachable(OverlayPhase::Recording)));
        assert!(!latch.observe(ObservedSignal::Reachable(OverlayPhase::Recording)));
    }

    #[test]
    fn red_a_transient_unreachable_blip_does_not_refire_the_recording_notification() {
        // Sol finding 1: the notify edge must come from OBSERVED daemon signals,
        // not rendered phases. A single failed status read renders the
        // "Daemon unavailable" capsule mid-Recording, but it is not a reachable
        // observation, so it must NOT reset the latch — otherwise the next
        // reachable Recording tick would refire the notification.
        let mut latch = RecordingNotifyLatch::default();
        assert!(latch.observe(ObservedSignal::Reachable(OverlayPhase::Recording)));
        assert!(!latch.observe(ObservedSignal::Unreachable));
        // Recording resumes after the blip: still latched, so no second notice.
        assert!(!latch.observe(ObservedSignal::Reachable(OverlayPhase::Recording)));
    }

    #[test]
    fn red_a_reachable_non_recording_state_rearms_the_recording_notification() {
        // Sol finding 1, second half: a genuine reachable non-Recording state
        // (Idle→Hidden, Processing, or a terminal event) DOES reset the latch, so
        // the next distinct Recording session notifies again.
        for reset in [
            OverlayPhase::Hidden,
            OverlayPhase::Processing,
            OverlayPhase::Success,
            OverlayPhase::Failure,
        ] {
            let mut latch = RecordingNotifyLatch::default();
            assert!(latch.observe(ObservedSignal::Reachable(OverlayPhase::Recording)));
            assert!(!latch.observe(ObservedSignal::Reachable(reset)));
            assert!(
                latch.observe(ObservedSignal::Reachable(OverlayPhase::Recording)),
                "a reachable {reset:?} must re-arm the Recording notification",
            );
        }
    }

    #[test]
    fn red_a_surface_handoff_after_render_breaks_before_any_tracker_or_latch_mutation() {
        // Sol round-2 minor: the post-render_surface() `switched` guard must
        // break BEFORE the resurface tracker or notify latch observe the tick,
        // or a handed-off (retired) window could be re-presented and a duplicate
        // notification sent on the same tick. Proven by state: a Break tick must
        // leave both the tracker and the latch untouched.
        let recording = OverlayView::from_response(&overlay_status(DaemonState::Recording, None));
        let mut tracker = PresentationTracker::default();
        let mut latch = RecordingNotifyLatch::default();
        let mut no_speech_latch = NoSpeechNotifyLatch::default();
        let mut limit_latch = LimitWarningLatch::default();
        // Prime both to a known state: tracker's last_phase = Recording, latch latched.
        assert!(tracker.observe(recording));
        assert!(latch.observe(ObservedSignal::Reachable(OverlayPhase::Recording)));

        // A tick where the realize callback handed off (switched_after_render =
        // true) must Break — even on the fallback path with a fresh visible view
        // that would otherwise resurface and notify.
        let action = poll_tick(
            true,
            true,
            OverlayView::HIDDEN,
            ObservedSignal::Reachable(OverlayPhase::Hidden),
            None,
            None,
            &mut tracker,
            &mut latch,
            &mut no_speech_latch,
            &mut limit_latch,
        );
        assert_eq!(action, TickAction::Break);

        // No mutation: had the guard run the tracker on HIDDEN, last_phase would
        // be Hidden and the next Recording would count as a fresh transition
        // (true). Had it run the latch on a reachable Hidden, the latch would
        // reset and the next Recording would refire (true). Both staying false
        // proves poll_tick broke before touching either.
        assert!(!tracker.observe(recording));
        assert!(!latch.observe(ObservedSignal::Reachable(OverlayPhase::Recording)));
        // Same for the limit latch: a Break tick must not consume a warning.
        assert_eq!(limit_latch.fired(), None);
    }

    #[test]
    fn red_a_live_fallback_tick_resurfaces_and_notifies_on_a_recording_edge() {
        // Companion to the guard test: with no handoff, a fallback Recording-edge
        // tick both resurfaces and notifies; a non-fallback tick runs neither.
        let recording = OverlayView::from_response(&overlay_status(DaemonState::Recording, None));
        let signal = ObservedSignal::Reachable(OverlayPhase::Recording);
        let mut tracker = PresentationTracker::default();
        let mut latch = RecordingNotifyLatch::default();
        let mut no_speech_latch = NoSpeechNotifyLatch::default();
        let mut limit_latch = LimitWarningLatch::default();
        assert_eq!(
            poll_tick(
                false, true, recording, signal, None, None,
                &mut tracker, &mut latch, &mut no_speech_latch, &mut limit_latch,
            ),
            TickAction::Continue {
                resurface: true, notify: true, notify_no_speech: false, notify_limit: None,
            },
        );
        // The layer-shell (non-fallback) path never resurfaces or notifies here.
        let mut tracker = PresentationTracker::default();
        let mut latch = RecordingNotifyLatch::default();
        let mut no_speech_latch = NoSpeechNotifyLatch::default();
        let mut limit_latch = LimitWarningLatch::default();
        assert_eq!(
            poll_tick(
                false, false, recording, signal, None, None,
                &mut tracker, &mut latch, &mut no_speech_latch, &mut limit_latch,
            ),
            TickAction::Continue {
                resurface: false, notify: false, notify_no_speech: false, notify_limit: None,
            },
        );
    }

    #[test]
    fn no_speech_latch_fires_once_per_no_speech_episode() {
        let mut latch = NoSpeechNotifyLatch::default();
        assert!(latch.observe(ObservedSignal::Reachable(OverlayPhase::NoSpeech)));
        // Level-triggered repeats during the terminal linger stay silent.
        assert!(!latch.observe(ObservedSignal::Reachable(OverlayPhase::NoSpeech)));
        // An unreachable blip must not re-arm.
        assert!(!latch.observe(ObservedSignal::Unreachable));
        assert!(!latch.observe(ObservedSignal::Reachable(OverlayPhase::NoSpeech)));
        // A different reachable phase re-arms for the next episode.
        assert!(!latch.observe(ObservedSignal::Reachable(OverlayPhase::Hidden)));
        assert!(latch.observe(ObservedSignal::Reachable(OverlayPhase::NoSpeech)));
    }

    /// The desktop-notification fallback rung (voisu-overlay.rs's
    /// `notification_tick`) drives its no-speech decision through this same
    /// latch instead of a plain phase-transition check, precisely so a
    /// retained NoSpeech episode that survives one unreachable poll blip does
    /// not read as two separate transitions and fire twice.
    #[test]
    fn no_speech_latch_does_not_double_fire_across_an_unreachable_blip_mid_episode() {
        let mut latch = NoSpeechNotifyLatch::default();
        assert!(latch.observe(ObservedSignal::Reachable(OverlayPhase::NoSpeech)));
        // Daemon becomes briefly unreachable while the terminal capsule is
        // still lingering on-screen from the first observation.
        assert!(!latch.observe(ObservedSignal::Unreachable));
        // Daemon recovers before the terminal window expires: NoSpeech is
        // observed again for what is still the SAME episode. Must stay silent.
        assert!(!latch.observe(ObservedSignal::Reachable(OverlayPhase::NoSpeech)));
    }

    #[test]
    fn poll_tick_reports_no_speech_even_on_the_layer_shell_path() {
        let mut tracker = PresentationTracker::default();
        let mut notify_latch = RecordingNotifyLatch::default();
        let mut no_speech_latch = NoSpeechNotifyLatch::default();
        let mut limit_latch = LimitWarningLatch::default();
        let view = OverlayView::no_speech();
        // is_fallback == false is the layer-shell path: recording-notify stays
        // fallback-only, but the no-speech explanation must fire on BOTH paths.
        let action = poll_tick(
            false, false, view,
            ObservedSignal::Reachable(OverlayPhase::NoSpeech),
            None,
            None,
            &mut tracker, &mut notify_latch, &mut no_speech_latch, &mut limit_latch,
        );
        assert_eq!(
            action,
            TickAction::Continue {
                resurface: false, notify: false, notify_no_speech: true, notify_limit: None,
            },
        );
        // Break still wins over everything and consumes no latch state.
        let action = poll_tick(
            true, false, view,
            ObservedSignal::Reachable(OverlayPhase::NoSpeech),
            None,
            None,
            &mut tracker, &mut notify_latch, &mut no_speech_latch, &mut limit_latch,
        );
        assert_eq!(action, TickAction::Break);
    }

    #[test]
    fn red_bounded_overlay_restarts_stop_without_a_daemon_control_path() {
        // RED proof: this policy is pure and takes no daemon handle. Replacing
        // it with an unbounded retry loop or a daemon restart cannot satisfy
        // this contract test.
        let mut policy = crate::feedback::OverlayRestartPolicy::default();
        assert!(policy.record_failure(Duration::from_secs(0)).should_restart());
        assert!(policy.record_failure(Duration::from_secs(10)).should_restart());
        assert!(!policy.record_failure(Duration::from_secs(20)).should_restart());
        assert!(policy.record_failure(Duration::from_secs(51)).should_restart());
    }

    fn sample_event() -> OverlayEvent {
        event(1, OverlayOutcome::Delivered)
    }

    fn recording_response() -> Response {
        overlay_status(DaemonState::Recording, None)
    }

    fn processing_response() -> Response {
        overlay_status(DaemonState::Processing, None)
    }

    #[test]
    fn quality_failure_maps_to_no_speech_view() {
        let event = OverlayEvent { outcome: OverlayOutcome::QualityFailure, ..sample_event() };
        let view = OverlayView::from_terminal_event(&event);
        assert_eq!(view.phase, OverlayPhase::NoSpeech);
        assert_eq!(view.visible_label, "Didn't catch any speech");
        assert_eq!(view.accessible_label, "No speech detected; nothing was delivered");
    }

    #[test]
    fn capsule_text_is_empty_for_graphics_first_phases() {
        assert_eq!(OverlayView::from_response(&recording_response()).capsule_text(), "");
        assert_eq!(OverlayView::from_response(&processing_response()).capsule_text(), "");
        assert_eq!(OverlayView::success().capsule_text(), "");
        // Text-bearing phases keep their words on the capsule.
        assert_eq!(OverlayView::daemon_unavailable().capsule_text(), "Daemon unavailable");
        assert_eq!(OverlayView::no_speech().capsule_text(), "");
        // The notification rung still gets full labels everywhere.
        assert_eq!(OverlayView::from_response(&recording_response()).visible_label, "Recording");
        assert_eq!(OverlayView::success().visible_label, "Delivered");
    }

    #[test]
    fn success_glyph_is_a_checkmark_and_no_speech_has_none() {
        assert_eq!(phase_glyph(OverlayPhase::Success), "✓");
        assert_eq!(phase_glyph(OverlayPhase::NoSpeech), "");
        assert_eq!(phase_glyph(OverlayPhase::Failure), "⚠");
        assert_eq!(phase_glyph(OverlayPhase::Processing), "");
    }

    #[test]
    fn edge_falloff_is_dim_at_the_ends_and_full_at_center() {
        let first = edge_falloff_alpha(0, 20);
        let mid = edge_falloff_alpha(10, 20);
        let last = edge_falloff_alpha(19, 20);
        assert!((0.45..0.55).contains(&first), "outer bar should be ~0.45–0.55, got {first}");
        assert!((first - last).abs() < 0.05, "falloff must be symmetric");
        assert!(mid > 0.97, "center bar should be ~full opacity, got {mid}");
        // Monotone from edge to center — no ripples in the ramp.
        for i in 0..9 {
            assert!(edge_falloff_alpha(i, 20) <= edge_falloff_alpha(i + 1, 20) + 1e-9);
        }
    }

    #[test]
    fn resting_floor_reads_as_a_dotted_baseline_not_a_flatline() {
        let floor = resting_floor(38.0); // 40px meter minus the 2px inset
        assert!((floor - 3.8).abs() < 1e-9, "floor is 10% of drawable height");
        assert!(resting_floor(10.0) >= 1.5, "floor never collapses below the old 1.5px minimum");
        // Silence (level 0) sits exactly on the floor; full level fills the height.
        assert!((recording_bar_height(0, 38.0) - floor).abs() < 1e-9);
        assert!((recording_bar_height(255, 38.0) - 38.0).abs() < 1e-9);
        // Monotone in level.
        assert!(recording_bar_height(80, 38.0) < recording_bar_height(200, 38.0));
    }

    #[test]
    fn sweep_brightness_moves_and_respects_reduced_motion() {
        // Reduced motion: uniform raised brightness, time-independent.
        for index in [0, 7, 19] {
            assert!((sweep_brightness(index, 20, 0.3, true) - 0.6).abs() < 1e-9);
            assert!((sweep_brightness(index, 20, 5.3, true) - 0.6).abs() < 1e-9);
        }
        // Full motion: brightness peaks near the sweep position and the peak moves.
        let early_left = sweep_brightness(2, 20, 0.15, false);
        let early_right = sweep_brightness(17, 20, 0.15, false);
        assert!(early_left > early_right, "early in the pass the bump is on the left");
        let late_left = sweep_brightness(2, 20, 1.05, false);
        let late_right = sweep_brightness(17, 20, 1.05, false);
        assert!(late_right > late_left, "late in the pass the bump is on the right");
        // Everything stays in a sane [0.25, 1.0] display range.
        for index in 0..20 {
            for t in [0.0, 0.4, 0.8, 1.19] {
                let b = sweep_brightness(index, 20, t, false);
                assert!((0.25..=1.0).contains(&b), "b={b} at index={index} t={t}");
            }
        }
    }

    #[test]
    fn interpolate_bands_length_matches_count() {
        let bands = [7u8; 20];
        assert_eq!(interpolate_bands(&bands, VISUAL_BAR_COUNT).len(), VISUAL_BAR_COUNT);
        assert_eq!(interpolate_bands(&bands, 44).len(), 44);
        assert_eq!(interpolate_bands(&bands, 20).len(), 20);
    }

    #[test]
    fn interpolate_bands_is_identity_at_count_20() {
        let bands: [u8; 20] = std::array::from_fn(|i| (i * 13 % 256) as u8);
        let out = interpolate_bands(&bands, 20);
        for (i, &b) in bands.iter().enumerate() {
            assert!((out[i] - f64::from(b)).abs() < 1e-9, "identity broke at {i}");
        }
    }

    #[test]
    fn interpolate_bands_stays_in_range() {
        let bands: [u8; 20] = std::array::from_fn(|i| ((i * 37 + 11) % 256) as u8);
        for &v in &interpolate_bands(&bands, VISUAL_BAR_COUNT) {
            assert!((0.0..=255.0).contains(&v), "out of range: {v}");
        }
    }

    #[test]
    fn interpolate_bands_preserves_monotonic_ramp() {
        // A monotonically increasing input must stay monotonic after resampling.
        let bands: [u8; 20] = std::array::from_fn(|i| (i * 13) as u8); // 0,13,...,247
        let out = interpolate_bands(&bands, VISUAL_BAR_COUNT);
        for w in out.windows(2) {
            assert!(w[1] >= w[0] - 1e-9, "ramp not monotonic: {} -> {}", w[0], w[1]);
        }
    }

    #[test]
    fn interpolate_bands_endpoints_are_exactly_the_clamped_bands() {
        // For count 44, the first sample position is (0.5/44)*20 - 0.5 < 0 and the
        // last is (43.5/44)*20 - 0.5 > 19, so both clamp onto band 0 / band 19 and
        // the endpoints equal the first/last band EXACTLY (no half-band slack).
        let bands: [u8; 20] = std::array::from_fn(|i| (i * 13) as u8); // 0..=247
        let out = interpolate_bands(&bands, VISUAL_BAR_COUNT);
        assert!((out[0] - 0.0).abs() < 1e-9, "first bar hugs band 0 exactly");
        assert!(
            (out[VISUAL_BAR_COUNT - 1] - 247.0).abs() < 1e-9,
            "last bar hugs band 19 exactly"
        );
    }

    #[test]
    fn interpolate_bands_lerps_interior_44_bar_values_exactly() {
        // Sentinel: a single spike at band 10 so the exact lerp weights are legible.
        let mut bands = [0u8; 20];
        bands[10] = 255;
        let out = interpolate_bands(&bands, VISUAL_BAR_COUNT);
        // Visual bar 22: pos = (22.5/44)*20 - 0.5 = 107/11 = 9.7272..., so it lerps
        // band 9 (0) -> band 10 (255) with frac 8/11 -> 255 * 8/11 = 2040/11.
        assert!(
            (out[22] - 2040.0 / 11.0).abs() < 1e-9,
            "bar 22 = {}, want 2040/11",
            out[22]
        );
        // Visual bar 23: pos = (23.5/44)*20 - 0.5 = 112/11 = 10.1818..., lerps
        // band 10 (255) -> band 11 (0) with frac 2/11 -> 255 * 9/11 = 2295/11.
        assert!(
            (out[23] - 2295.0 / 11.0).abs() < 1e-9,
            "bar 23 = {}, want 2295/11",
            out[23]
        );
    }

    #[test]
    fn sweep_brightness_moves_across_the_44_bar_row() {
        // Reduced motion is uniform and time-independent at the visual count too.
        for index in [0, 22, 43] {
            assert!((sweep_brightness(index, VISUAL_BAR_COUNT, 0.3, true) - 0.6).abs() < 1e-9);
            assert!((sweep_brightness(index, VISUAL_BAR_COUNT, 5.3, true) - 0.6).abs() < 1e-9);
        }
        // Full motion: bump enters on the left early and reaches the right late.
        let early_left = sweep_brightness(4, VISUAL_BAR_COUNT, 0.15, false);
        let early_right = sweep_brightness(39, VISUAL_BAR_COUNT, 0.15, false);
        assert!(early_left > early_right, "early in the pass the bump is on the left");
        let late_left = sweep_brightness(4, VISUAL_BAR_COUNT, 1.05, false);
        let late_right = sweep_brightness(39, VISUAL_BAR_COUNT, 1.05, false);
        assert!(late_right > late_left, "late in the pass the bump is on the right");
        // Stays in the sane display range across the whole 44-bar row.
        for index in 0..VISUAL_BAR_COUNT {
            for t in [0.0, 0.4, 0.8, 1.19] {
                let b = sweep_brightness(index, VISUAL_BAR_COUNT, t, false);
                assert!((0.25..=1.0).contains(&b), "b={b} at index={index} t={t}");
            }
        }
    }

    // --- approaching-limit warnings -------------------------------------

    /// The ceiling the daemon actually enforces. Imported rather than repeated:
    /// if it ever moves, these tests move with it and a hardcoded warning
    /// threshold would fail here first.
    use crate::system::MAX_RECORDING_DURATION;

    fn recording() -> ObservedSignal {
        ObservedSignal::Reachable(OverlayPhase::Recording)
    }

    #[test]
    fn limit_warning_onsets_are_derived_from_the_real_recording_ceiling() {
        let (approaching, final_warning) = limit_warning_onsets(MAX_RECORDING_DURATION);
        assert_eq!(approaching, MAX_RECORDING_DURATION - APPROACHING_LIMIT_LEAD);
        assert_eq!(final_warning, MAX_RECORDING_DURATION - FINAL_LIMIT_LEAD);
        // Pinned in wall-clock terms too, so a silent change to either lead is
        // visible as the product behaviour the spec describes: 9:00 and 9:50.
        assert_eq!(approaching, Duration::from_secs(540));
        assert_eq!(final_warning, Duration::from_secs(590));
        // And the stages agree with the onsets at the boundary.
        assert_eq!(limit_warning_at(approaching - Duration::from_millis(1), MAX_RECORDING_DURATION), None);
        assert_eq!(
            limit_warning_at(approaching, MAX_RECORDING_DURATION),
            Some(LimitWarning::Approaching)
        );
        assert_eq!(
            limit_warning_at(final_warning, MAX_RECORDING_DURATION),
            Some(LimitWarning::Final)
        );
        assert_eq!(
            limit_warning_at(MAX_RECORDING_DURATION, MAX_RECORDING_DURATION),
            Some(LimitWarning::Final)
        );
    }

    #[test]
    fn limit_warnings_track_an_overridden_ceiling_rather_than_a_literal() {
        // VOISU_RECORDING_DEADLINE_MS may shorten a Recording. The warnings
        // must follow it, not the 600 s default: at a 120 s ceiling the first
        // warning belongs at 60 s, not at 540 s (which would never fire).
        let ceiling = Duration::from_secs(120);
        assert_eq!(
            limit_warning_onsets(ceiling),
            (Duration::from_secs(60), Duration::from_secs(110))
        );
        assert_eq!(limit_warning_at(Duration::from_secs(59), ceiling), None);
        assert_eq!(limit_warning_at(Duration::from_secs(60), ceiling), Some(LimitWarning::Approaching));
        assert_eq!(limit_warning_at(Duration::from_secs(110), ceiling), Some(LimitWarning::Final));
        // A ceiling shorter than a lead saturates instead of wrapping.
        let tiny = Duration::from_secs(5);
        assert_eq!(limit_warning_onsets(tiny), (Duration::ZERO, Duration::ZERO));
        assert_eq!(limit_warning_at(Duration::ZERO, tiny), Some(LimitWarning::Final));
    }

    #[test]
    fn limit_warning_reads_the_headroom_the_daemon_reports() {
        assert_eq!(limit_warning_for_remaining(None), None);
        assert_eq!(limit_warning_for_remaining(Some(Duration::from_secs(61))), None);
        assert_eq!(
            limit_warning_for_remaining(Some(APPROACHING_LIMIT_LEAD)),
            Some(LimitWarning::Approaching)
        );
        assert_eq!(
            limit_warning_for_remaining(Some(FINAL_LIMIT_LEAD)),
            Some(LimitWarning::Final)
        );
        assert_eq!(
            limit_warning_for_remaining(Some(Duration::ZERO)),
            Some(LimitWarning::Final)
        );
    }

    #[test]
    fn each_limit_warning_fires_exactly_once_across_many_overlay_ticks() {
        // Time is driven synthetically: the overlay polls every 200 ms, so one
        // simulated Recording is 3000 ticks of a 600 s ceiling. No sleeping.
        let mut latch = LimitWarningLatch::default();
        let mut fired = Vec::new();
        for tick in 0..=3_000u64 {
            let elapsed = Duration::from_millis(200 * tick);
            let warning = limit_warning_at(elapsed, MAX_RECORDING_DURATION);
            if let Some(announced) = latch.observe(recording(), None, warning) {
                fired.push((elapsed, announced));
            }
        }
        assert_eq!(
            fired,
            vec![
                (Duration::from_secs(540), LimitWarning::Approaching),
                (Duration::from_secs(590), LimitWarning::Final),
            ],
            "each stage announces once, at its derived onset"
        );
    }

    #[test]
    fn an_unreachable_blip_mid_recording_never_replays_a_warning() {
        let mut latch = LimitWarningLatch::default();
        assert_eq!(
            latch.observe(recording(), None, Some(LimitWarning::Approaching)),
            Some(LimitWarning::Approaching)
        );
        // A failed status read is not an observation of a new Recording.
        assert_eq!(
            latch.observe(ObservedSignal::Unreachable, None, Some(LimitWarning::Approaching)),
            None
        );
        assert_eq!(latch.fired(), Some(LimitWarning::Approaching));
        assert_eq!(latch.observe(recording(), None, Some(LimitWarning::Approaching)), None);
        // Escalation still gets through once.
        assert_eq!(
            latch.observe(recording(), None, Some(LimitWarning::Final)),
            Some(LimitWarning::Final)
        );
        assert_eq!(latch.observe(recording(), None, Some(LimitWarning::Final)), None);
    }

    /// The poll is 200 ms and a stop plus a restart can both complete inside
    /// one gap, so the observer can see Recording immediately followed by
    /// Recording with no phase between them. Keying on phase alone, the second
    /// Recording would inherit the first one's fired stages and never warn.
    #[test]
    fn a_new_recording_warns_again_even_with_no_phase_change_between_them() {
        let mut latch = LimitWarningLatch::default();
        assert_eq!(
            latch.observe(recording(), Some("correlation-1"), Some(LimitWarning::Approaching)),
            Some(LimitWarning::Approaching)
        );
        assert_eq!(
            latch.observe(recording(), Some("correlation-1"), Some(LimitWarning::Final)),
            Some(LimitWarning::Final)
        );
        // A different Recording, observed back to back with the first.
        assert_eq!(
            latch.observe(recording(), Some("correlation-2"), Some(LimitWarning::Approaching)),
            Some(LimitWarning::Approaching),
            "the next Recording must warn from scratch"
        );
        assert_eq!(
            latch.observe(recording(), Some("correlation-2"), Some(LimitWarning::Approaching)),
            None,
            "and must still be latched within itself"
        );
        // An unreachable poll is not an identity change, so nothing replays.
        assert_eq!(
            latch.observe(ObservedSignal::Unreachable, None, Some(LimitWarning::Approaching)),
            None
        );
        assert_eq!(
            latch.observe(recording(), Some("correlation-2"), Some(LimitWarning::Approaching)),
            None
        );
    }

    #[test]
    fn recording_identity_comes_from_the_correlation_id_already_on_the_wire() {
        let idle = overlay_status(DaemonState::Idle, None);
        assert_eq!(recording_identity(&idle), None);
        // Deserialized from the wire shape the daemon actually sends, so this
        // also pins that the identity survives the round trip.
        let recording: Response = serde_json::from_str(
            r#"{"version":1,"ok":true,"state":"recording","message":"recording",
                "evidence":{"recording_id":7,"correlation_id":"correlation-7",
                "stages":[],"delivery_count":0}}"#,
        )
        .unwrap();
        assert_eq!(recording_identity(&recording), Some("correlation-7"));
        // An evidence-free reply (Starting, or an older daemon) has no identity
        // to key on, which must read as "unknown", never as a shared one.
        let mut blank = recording_response();
        blank.evidence = None;
        assert_eq!(recording_identity(&blank), None);
    }

    #[test]
    fn a_short_recording_warns_nothing_and_leaves_no_latch_residue() {
        let mut latch = LimitWarningLatch::default();
        // Fifteen seconds of Recording against the real ceiling: no warning.
        for tick in 0..75u64 {
            let elapsed = Duration::from_millis(200 * tick);
            let warning = limit_warning_at(elapsed, MAX_RECORDING_DURATION);
            assert_eq!(warning, None);
            assert_eq!(latch.observe(recording(), None, warning), None);
        }
        // Then the Recording ends: Processing, then Idle (Hidden).
        assert_eq!(latch.observe(ObservedSignal::Reachable(OverlayPhase::Processing), None, None), None);
        assert_eq!(latch.observe(ObservedSignal::Reachable(OverlayPhase::Hidden), None, None), None);
        assert_eq!(latch.fired(), None, "no residue survives the Recording");
        // A long Recording that warns must also leave the latch clean.
        assert_eq!(
            latch.observe(recording(), None, Some(LimitWarning::Final)),
            Some(LimitWarning::Final)
        );
        assert_eq!(latch.observe(ObservedSignal::Reachable(OverlayPhase::Success), None, None), None);
        assert_eq!(latch.fired(), None);
        // The next Recording therefore warns again from scratch.
        assert_eq!(
            latch.observe(recording(), None, Some(LimitWarning::Approaching)),
            Some(LimitWarning::Approaching)
        );
    }

    #[test]
    fn a_status_reply_without_headroom_is_never_a_warning() {
        // Idle, and any daemon that predates the field, report nothing. The
        // absent field must read as "no warning", not as zero headroom.
        let idle = overlay_status(DaemonState::Idle, None);
        assert_eq!(recording_remaining(&idle), None);
        assert_eq!(limit_warning_from_response(&idle), None);
        let mut recording = recording_response();
        recording.recording_remaining_ms = Some(120_000);
        assert_eq!(recording_remaining(&recording), Some(Duration::from_secs(120)));
        assert_eq!(limit_warning_from_response(&recording), None);
        recording.recording_remaining_ms = Some(45_000);
        assert_eq!(limit_warning_from_response(&recording), Some(LimitWarning::Approaching));
        recording.recording_remaining_ms = Some(4_000);
        assert_eq!(limit_warning_from_response(&recording), Some(LimitWarning::Final));
    }

    #[test]
    fn poll_tick_announces_a_limit_warning_once_on_both_windowed_paths() {
        for is_fallback in [false, true] {
            let mut tracker = PresentationTracker::default();
            let mut notify_latch = RecordingNotifyLatch::default();
            let mut no_speech_latch = NoSpeechNotifyLatch::default();
            let mut limit_latch = LimitWarningLatch::default();
            let view = OverlayView::from_response(&recording_response());
            let signal = recording();
            let mut announced = Vec::new();
            // Twenty ticks inside the approaching window, then twenty inside
            // the final one — the cadence a live 200 ms poll would produce.
            for warning in std::iter::repeat_n(Some(LimitWarning::Approaching), 20)
                .chain(std::iter::repeat_n(Some(LimitWarning::Final), 20))
            {
                match poll_tick(
                    false, is_fallback, view, signal, None, warning,
                    &mut tracker, &mut notify_latch, &mut no_speech_latch, &mut limit_latch,
                ) {
                    TickAction::Continue { notify_limit, .. } => announced.extend(notify_limit),
                    TickAction::Break => unreachable!("no handoff in this test"),
                }
            }
            assert_eq!(
                announced,
                vec![LimitWarning::Approaching, LimitWarning::Final],
                "is_fallback={is_fallback}"
            );
        }
    }

    #[test]
    fn the_capsule_turns_amber_for_both_stages_and_borders_only_at_the_final_one() {
        assert_eq!(recording_bar_rgb(None), RECORDING_BAR_RGB);
        assert_eq!(recording_bar_rgb(Some(LimitWarning::Approaching)), LIMIT_WARNING_BAR_RGB);
        // Amber bars are RETAINED at the final stage; the border is additive.
        assert_eq!(recording_bar_rgb(Some(LimitWarning::Final)), LIMIT_WARNING_BAR_RGB);
        assert_eq!(limit_warning_class(None), None);
        assert_eq!(limit_warning_class(Some(LimitWarning::Approaching)), None);
        assert_eq!(limit_warning_class(Some(LimitWarning::Final)), Some(LIMIT_WARNING_CLASS));
    }

    #[test]
    fn limit_warning_bodies_are_the_approved_notification_text() {
        assert_eq!(
            limit_warning_body(LimitWarning::Approaching),
            "Approaching the recording limit — about a minute left"
        );
        assert_eq!(limit_warning_body(LimitWarning::Final), "Recording stops in 10 seconds");
        // At the real ceiling the approved wording is what actually goes out:
        // the first poll inside each window still rounds to the nominal lead.
        let (approaching, final_warning) = limit_warning_onsets(MAX_RECORDING_DURATION);
        let first_approaching_tick = MAX_RECORDING_DURATION - approaching - TICK;
        assert_eq!(
            limit_notification_body(LimitWarning::Approaching, first_approaching_tick).as_deref(),
            Some("Approaching the recording limit — about a minute left")
        );
        let first_final_tick = MAX_RECORDING_DURATION - final_warning - TICK;
        assert_eq!(
            limit_notification_body(LimitWarning::Final, first_final_tick).as_deref(),
            Some("Recording stops in 10 seconds")
        );
    }

    /// The Overlay's poll cadence, so the "first tick inside the window" cases
    /// above are the real ones rather than an idealised exact boundary.
    const TICK: Duration = Duration::from_millis(200);

    /// `VOISU_RECORDING_DEADLINE_MS` is a real operator knob, not a test seam,
    /// so a ceiling shorter than a lead is reachable in production. The wording
    /// must never promise time the Recording does not have.
    #[test]
    fn a_short_ceiling_is_told_the_truth_instead_of_the_nominal_wording() {
        // A 30 s ceiling: the approaching warning is live from the first tick.
        let ceiling = Duration::from_secs(30);
        assert_eq!(limit_warning_at(Duration::ZERO, ceiling), Some(LimitWarning::Approaching));
        assert_eq!(
            limit_notification_body(LimitWarning::Approaching, ceiling).as_deref(),
            Some("Approaching the recording limit — about 30 seconds left"),
            "a 30 s ceiling must not claim a minute"
        );
        // A 5 s ceiling: the final warning is live from the first tick.
        let ceiling = Duration::from_secs(5);
        assert_eq!(limit_warning_at(Duration::ZERO, ceiling), Some(LimitWarning::Final));
        assert_eq!(
            limit_notification_body(LimitWarning::Final, ceiling).as_deref(),
            Some("Recording stops in 5 seconds"),
            "a 5 s ceiling must not promise ten"
        );
        // Singular reads as English, not "1 seconds".
        assert_eq!(
            limit_notification_body(LimitWarning::Final, Duration::from_secs(1)).as_deref(),
            Some("Recording stops in 1 second")
        );
    }

    /// The Notifier rung can carry exactly one bubble per tick, so the two
    /// sends that used to be able to come due together had to be resolved into
    /// one choice — and the warning is the one with a deadline attached.
    #[test]
    fn the_notification_rung_sends_the_warning_rather_than_the_transition() {
        let recording = OverlayView::from_response(&recording_response());
        // The tick where the Overlay first sees an already-warning Recording:
        // a transition into Recording AND a due warning, on the same tick.
        assert_eq!(
            notification_rung_choice(recording, OverlayPhase::Hidden, true, false),
            Some(RungNotification::Limit),
            "a due warning must not be dropped or painted over by the transition"
        );
        // With no warning due, the transition is announced exactly as before.
        assert_eq!(
            notification_rung_choice(recording, OverlayPhase::Hidden, false, false),
            Some(RungNotification::Label("Recording"))
        );
        // And a repeat of the same phase stays silent.
        assert_eq!(
            notification_rung_choice(recording, OverlayPhase::Recording, false, false),
            None
        );
        // No-speech keeps its latch-gated explanation, and still loses to a
        // warning if both somehow come due.
        let no_speech = OverlayView::no_speech();
        assert_eq!(
            notification_rung_choice(no_speech, OverlayPhase::Recording, false, true),
            Some(RungNotification::Label(no_speech.visible_label))
        );
        assert_eq!(
            notification_rung_choice(no_speech, OverlayPhase::Recording, false, false),
            None,
            "an unlatched no-speech repeat stays silent"
        );
        assert_eq!(
            notification_rung_choice(no_speech, OverlayPhase::Recording, true, true),
            Some(RungNotification::Limit)
        );
        // Hidden announces nothing.
        assert_eq!(
            notification_rung_choice(OverlayView::HIDDEN, OverlayPhase::Recording, false, false),
            None
        );
    }

    /// The Notifier's channel is depth-1 with a non-blocking send, so a Notify
    /// call already in flight makes the send fail. A warning is chosen once and
    /// the latch would commit it, so a dropped send used to mean silence for
    /// the rest of the Recording. The stage is committed only once the sink
    /// accepts it.
    #[test]
    fn a_warning_the_notifier_refuses_is_re_announced_on_a_later_tick() {
        let mut latch = LimitWarningLatch::default();
        let view = OverlayView::from_response(&recording_response());
        let mut previous_phase = OverlayPhase::Hidden;
        // The sink refuses the first three attempts, standing in for a full
        // depth-1 channel with a D-Bus call in flight.
        let mut refusals_left = 3;
        let mut announced = Vec::new();
        // Sixty ticks (twelve seconds at the 200 ms poll) of a Recording that
        // is inside the approaching window throughout.
        for _ in 0..60 {
            let chosen = latch.observe(recording(), Some("correlation-1"), Some(LimitWarning::Approaching));
            let body = chosen.and_then(|warning| {
                limit_notification_body(warning, Duration::from_secs(30))
            });
            if let Some(RungNotification::Limit) =
                notification_rung_choice(view, previous_phase, body.is_some(), false)
                && let Some(body) = body
            {
                let accepted = refusals_left == 0;
                refusals_left -= i32::from(refusals_left > 0);
                if accepted {
                    announced.push(body);
                } else {
                    latch.rollback();
                }
            }
            previous_phase = view.phase;
        }
        assert_eq!(
            announced,
            vec!["Approaching the recording limit — about 30 seconds left".to_owned()],
            "a refused warning must survive and be announced exactly once"
        );
    }

    /// The rollback must not resurrect a stage the Recording has already moved
    /// past: a returned `Approaching` that is now inside the final window is
    /// announced as `Final` alone.
    #[test]
    fn a_rolled_back_warning_escalates_rather_than_announcing_both_stages() {
        let mut latch = LimitWarningLatch::default();
        let identity = Some("correlation-1");
        assert_eq!(
            latch.observe(recording(), identity, Some(LimitWarning::Approaching)),
            Some(LimitWarning::Approaching)
        );
        // The sink refused it.
        latch.rollback();
        assert_eq!(latch.fired(), None);
        // By the next tick the Recording is in the final window.
        assert_eq!(
            latch.observe(recording(), identity, Some(LimitWarning::Final)),
            Some(LimitWarning::Final)
        );
        assert_eq!(
            latch.observe(recording(), identity, Some(LimitWarning::Final)),
            None,
            "and the stale approaching stage never comes back"
        );
        // A rollback with nothing outstanding, or a repeated one, is inert.
        latch.rollback();
        latch.rollback();
        assert_eq!(
            latch.observe(recording(), identity, Some(LimitWarning::Final)),
            None
        );
    }

    #[test]
    fn zero_headroom_says_nothing_at_all() {
        // A status reply that arrives while the stop is already in flight. The
        // stage is still Final (the capsule stays bordered), but counting down
        // to a moment that has passed is worse than silence.
        assert_eq!(limit_warning_for_remaining(Some(Duration::ZERO)), Some(LimitWarning::Final));
        assert_eq!(limit_notification_body(LimitWarning::Final, Duration::ZERO), None);
        assert_eq!(
            limit_notification_body(LimitWarning::Final, Duration::from_millis(400)),
            None,
            "rounding to zero seconds is the same moment"
        );
        assert_eq!(
            limit_notification_body(LimitWarning::Approaching, Duration::ZERO),
            None
        );
    }
}
