//! Presentation-only state derived from the daemon's public observer response.
//! This module owns no Recording, provider, or Delivery work.

use std::time::{Duration, Instant};

use voisu_core::{DaemonState, OverlayEvent, OverlayOutcome, Response};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OverlayPhase {
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

    pub const fn is_visible(self) -> bool { !matches!(self.phase, OverlayPhase::Hidden) }

    pub const fn animation_interval(self, reduced_motion: bool) -> Option<Duration> {
        if reduced_motion || !matches!(self.phase, OverlayPhase::Recording) { None }
        else { Some(Duration::from_millis(160)) }
    }
}

const TERMINAL_DISPLAY: Duration = Duration::from_secs(2);

#[derive(Debug, Default)]
pub struct PresentationController {
    displayed_event: Option<u64>,
    terminal_until: Option<Instant>,
}

impl PresentationController {
    pub fn observe(&mut self, response: &Response, now: Instant) -> OverlayView {
        if matches!(response.state, Some(DaemonState::Recording)) {
            self.terminal_until = None;
            return OverlayView::from_response(response);
        }
        if let Some(event) = response.overlay_event.as_ref()
            && self.displayed_event != Some(event.id)
        {
            self.displayed_event = Some(event.id);
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use voisu_core::{DaemonState, OverlayEvent, OverlayOutcome, Response};

    fn event(id: u64, outcome: OverlayOutcome) -> OverlayEvent {
        OverlayEvent { id, outcome, message: "exact public outcome".into() }
    }

    #[test]
    fn public_observer_response_is_typed_and_terminal_events_are_displayed_once() {
        let terminal = Response { version: 1, ok: true, state: Some(DaemonState::Idle),
            message: "idle".into(), evidence: None, history: None, export: None,
            overlay_event: Some(event(7, OverlayOutcome::DeliveryFailure)) };
        let mut controller = PresentationController::default();
        let now = Instant::now();
        assert_eq!(controller.observe(&terminal, now).phase, OverlayPhase::Failure);
        assert_eq!(controller.observe(&terminal, now).phase, OverlayPhase::Failure);
        assert_eq!(controller.observe(&terminal, now + TERMINAL_DISPLAY).phase, OverlayPhase::Hidden);
    }

    #[test]
    fn next_recording_clears_terminal_feedback_and_is_not_lifecycle_coupled() {
        let mut controller = PresentationController::default();
        let now = Instant::now();
        let terminal = Response { version: 1, ok: true, state: Some(DaemonState::Idle),
            message: "idle".into(), evidence: None, history: None, export: None,
            overlay_event: Some(event(1, OverlayOutcome::QualityFailure)) };
        assert_eq!(controller.observe(&terminal, now).phase, OverlayPhase::Failure);
        let recording = Response::success(DaemonState::Recording, "Recording");
        assert_eq!(controller.observe(&recording, now).phase, OverlayPhase::Recording);
        assert_eq!(controller.observe(&Response::success(DaemonState::Idle, "idle"), now).phase,
            OverlayPhase::Hidden);
    }

    #[test]
    fn responses_from_a_pre_event_daemon_are_safe_and_have_no_stale_feedback() {
        let response: Response = serde_json::from_str(
            r#"{"version":1,"ok":true,"state":"idle","message":"idle"}"#,
        ).unwrap();
        assert!(response.overlay_event.is_none());
        assert_eq!(PresentationController::default().observe(&response, Instant::now()),
            OverlayView::HIDDEN);
    }
}
