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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OverlayView {
    pub phase: OverlayPhase,
    pub activity: u8,
    pub visible_label: &'static str,
    pub accessible_label: &'static str,
}

impl OverlayView {
    pub const HIDDEN: Self = Self {
        phase: OverlayPhase::Hidden,
        activity: 0,
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
                activity: response.evidence.as_ref()
                    .map(|e| e.streamed_chunk_count.min(3) as u8).unwrap_or(1),
                visible_label: "Recording",
                accessible_label: "Recording; voice activity visible",
            },
            Some(DaemonState::Processing) => Self {
                phase: OverlayPhase::Processing,
                activity: 0,
                visible_label: "Processing",
                accessible_label: "Processing Recording",
            },
            Some(DaemonState::Idle) | None => Self::HIDDEN,
        }
    }

    pub const fn from_terminal_event(event: &OverlayEvent) -> Self {
        match event.outcome {
            OverlayOutcome::Delivered => Self { phase: OverlayPhase::Success, activity: 0,
                visible_label: "Delivered", accessible_label: "Transcript Delivered" },
            OverlayOutcome::QualityFailure => Self::failure(),
            _ => Self { phase: OverlayPhase::Failure, activity: 0,
                visible_label: "Failure", accessible_label: "Recording failed" },
        }
    }

    pub const fn success() -> Self {
        Self { phase: OverlayPhase::Success, activity: 0,
            visible_label: "Delivered", accessible_label: "Transcript Delivered" }
    }

    pub const fn failure() -> Self {
        Self { phase: OverlayPhase::Failure, activity: 0,
            visible_label: "Quality Failure", accessible_label: "Quality Failure" }
    }

    /// The Failure view shown when the optional Overlay cannot reach the
    /// daemon. Owned here so the label strings live in one place; the label
    /// text is load-bearing for tests and users and must stay unchanged.
    pub const fn daemon_unavailable() -> Self {
        Self {
            phase: OverlayPhase::Failure,
            activity: 0,
            visible_label: "Daemon unavailable",
            accessible_label: "Daemon unavailable; the optional Overlay cannot reach voisu-daemon",
        }
    }

    pub const fn is_visible(self) -> bool { !matches!(self.phase, OverlayPhase::Hidden) }

    pub const fn animation_interval(self, reduced_motion: bool) -> Option<Duration> {
        if reduced_motion || !matches!(self.phase, OverlayPhase::Recording) { None }
        else { Some(Duration::from_millis(160)) }
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

/// The outcome of one fallback-path poll tick, decided purely so the adapter's
/// side effects (`window.present()`, `send_notification`) stay a thin match.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TickAction {
    /// Stop driving the window this tick and break the poll loop.
    Break,
    /// Keep polling; `resurface`/`notify` say which side effects to run.
    Continue { resurface: bool, notify: bool },
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
    tracker: &mut PresentationTracker,
    notify_latch: &mut RecordingNotifyLatch,
) -> TickAction {
    if switched_after_render {
        return TickAction::Break;
    }
    if !is_fallback {
        return TickAction::Continue { resurface: false, notify: false };
    }
    let resurface = tracker.observe(view);
    let notify = notify_latch.observe(signal);
    TickAction::Continue { resurface, notify }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feedback::{select_feedback_backend, FeedbackBackend, FeedbackCapabilities, SessionKind};
    use voisu_core::{DaemonState, OverlayEvent, OverlayOutcome, Response, VersionEnvelope};

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
        assert_eq!(controller.observe(&terminal, now).phase, OverlayPhase::Failure);
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
                FeedbackBackend::RegularSurface,
                Some(crate::feedback::FeedbackDegradation::X11),
            ),
            (
                FeedbackCapabilities { session: SessionKind::Wayland, display_available: true, xwayland_fallback: false, layer_shell_supported: false },
                FeedbackBackend::RegularSurface,
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
            &mut tracker,
            &mut latch,
        );
        assert_eq!(action, TickAction::Break);

        // No mutation: had the guard run the tracker on HIDDEN, last_phase would
        // be Hidden and the next Recording would count as a fresh transition
        // (true). Had it run the latch on a reachable Hidden, the latch would
        // reset and the next Recording would refire (true). Both staying false
        // proves poll_tick broke before touching either.
        assert!(!tracker.observe(recording));
        assert!(!latch.observe(ObservedSignal::Reachable(OverlayPhase::Recording)));
    }

    #[test]
    fn red_a_live_fallback_tick_resurfaces_and_notifies_on_a_recording_edge() {
        // Companion to the guard test: with no handoff, a fallback Recording-edge
        // tick both resurfaces and notifies; a non-fallback tick runs neither.
        let recording = OverlayView::from_response(&overlay_status(DaemonState::Recording, None));
        let signal = ObservedSignal::Reachable(OverlayPhase::Recording);
        let mut tracker = PresentationTracker::default();
        let mut latch = RecordingNotifyLatch::default();
        assert_eq!(
            poll_tick(false, true, recording, signal, &mut tracker, &mut latch),
            TickAction::Continue { resurface: true, notify: true },
        );
        // The layer-shell (non-fallback) path never resurfaces or notifies here.
        let mut tracker = PresentationTracker::default();
        let mut latch = RecordingNotifyLatch::default();
        assert_eq!(
            poll_tick(false, false, recording, signal, &mut tracker, &mut latch),
            TickAction::Continue { resurface: false, notify: false },
        );
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
}
