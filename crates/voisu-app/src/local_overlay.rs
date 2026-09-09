//! Overlay/feedback presentation for Local pending vs active, Loading, and stalls.

use std::sync::atomic::{AtomicBool, Ordering};

use voisu_core::{AsrMode, DaemonState, LocalReadiness, Response};

use crate::overlay::{OverlayPhase, OverlayView};

/// Caps OverlayStatus IPC at one in-flight round trip.
pub struct StatusPollGate {
    in_flight: AtomicBool,
}

impl StatusPollGate {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            in_flight: AtomicBool::new(false),
        }
    }

    #[must_use]
    pub fn try_begin(&self) -> bool {
        self.in_flight
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    pub fn end(&self) {
        self.in_flight.store(false, Ordering::SeqCst);
    }
}

impl Default for StatusPollGate {
    fn default() -> Self {
        Self::new()
    }
}

pub struct StatusPollGuard<'a> {
    gate: &'a StatusPollGate,
}

impl<'a> StatusPollGuard<'a> {
    #[must_use]
    pub fn acquire(gate: &'a StatusPollGate) -> Option<Self> {
        gate.try_begin().then_some(Self { gate })
    }
}

impl Drop for StatusPollGuard<'_> {
    fn drop(&mut self) {
        self.gate.end();
    }
}

#[must_use]
pub fn global_status_poll_gate() -> &'static StatusPollGate {
    static GATE: StatusPollGate = StatusPollGate::new();
    &GATE
}

/// Duplicate Trigger Key while the same Recording is already active.
#[derive(Debug, Default)]
pub struct TriggerRepeatLatch {
    last_identity: Option<String>,
}

impl TriggerRepeatLatch {
    /// Returns true once per Recording identity. Repeats of the same identity
    /// (key repeat / duplicate trigger) stay silent.
    #[must_use]
    pub fn observe(&mut self, identity: Option<&str>) -> bool {
        match identity {
            Some(id) if self.last_identity.as_deref() == Some(id) => false,
            Some(id) => {
                self.last_identity = Some(id.to_owned());
                true
            }
            None => {
                self.last_identity = None;
                false
            }
        }
    }
}

#[must_use]
pub fn view_from_response(response: &Response) -> OverlayView {
    if !response.ok {
        return OverlayView::failure();
    }
    match response.state {
        Some(DaemonState::Recording) => OverlayView {
            phase: OverlayPhase::Recording,
            visible_label: "Recording",
            accessible_label: recording_accessible(response),
        },
        Some(DaemonState::Processing) => OverlayView {
            phase: OverlayPhase::Processing,
            visible_label: "Processing",
            accessible_label: processing_accessible(response),
        },
        Some(DaemonState::Idle) | None => idle_local_view(response),
    }
}

#[must_use]
pub fn screen_reader_announcement(view: OverlayView) -> &'static str {
    view.accessible_label
}

#[must_use]
pub fn daemon_stall_announcement() -> &'static str {
    OverlayView::daemon_unavailable().accessible_label
}

fn recording_accessible(response: &Response) -> &'static str {
    match response.asr_mode.as_ref() {
        Some(asr) if asr.active == Some(AsrMode::Cloud) && asr.pending == Some(AsrMode::Local) => {
            "Recording on Cloud; Local is pending and does not apply to this Recording; voice activity visible"
        }
        Some(asr) if asr.active == Some(AsrMode::Local) => {
            "Recording on Local; voice activity visible"
        }
        Some(asr) if asr.active == Some(AsrMode::Cloud) => {
            "Recording on Cloud; voice activity visible"
        }
        _ => "Recording; voice activity visible",
    }
}

fn processing_accessible(response: &Response) -> &'static str {
    match response.asr_mode.as_ref() {
        Some(asr) if asr.active == Some(AsrMode::Local) => {
            "Processing Local Recording; Overlay status stays responsive during inference"
        }
        _ => "Processing Recording",
    }
}

fn idle_local_view(response: &Response) -> OverlayView {
    let Some(asr) = response.asr_mode.as_ref() else {
        return OverlayView::HIDDEN;
    };
    if asr.pending != Some(AsrMode::Local) {
        return OverlayView::HIDDEN;
    }
    match &asr.local_readiness {
        LocalReadiness::Loading | LocalReadiness::Verifying => OverlayView {
            phase: OverlayPhase::Failure,
            visible_label: "Loading",
            accessible_label: "Local selected; model loading; Start is refused until ready; no deferred capture queue",
        },
        LocalReadiness::Unavailable { .. } => OverlayView {
            phase: OverlayPhase::Failure,
            visible_label: "Local selected; model unavailable",
            accessible_label: "Local selected; model unavailable; Start is refused before capture",
        },
        LocalReadiness::Busy | LocalReadiness::Stopping => OverlayView {
            phase: OverlayPhase::Processing,
            visible_label: "Processing",
            accessible_label: "Local model is busy; status remains responsive",
        },
        LocalReadiness::Ready { .. } | LocalReadiness::Absent => OverlayView::HIDDEN,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use voisu_core::{AsrModeStatus, DaemonState, LocalReadiness, Response};

    fn response(state: DaemonState, asr: Option<AsrModeStatus>) -> Response {
        let mut response = Response::success(state, state.cli_label());
        response.asr_mode = asr;
        response
    }

    fn asr(pending: AsrMode, active: Option<AsrMode>, readiness: LocalReadiness) -> AsrModeStatus {
        AsrModeStatus {
            pending: Some(pending),
            active,
            revision: Some(1),
            local_readiness: readiness,
            admission_error: None,
            audio_retention: None,
            local_path_clean: None,
        }
    }

    #[test]
    fn pending_local_does_not_mark_a_cloud_recording_offline() {
        let view = view_from_response(&response(
            DaemonState::Recording,
            Some(asr(
                AsrMode::Local,
                Some(AsrMode::Cloud),
                LocalReadiness::Loading,
            )),
        ));
        assert_eq!(view.phase, OverlayPhase::Recording);
        assert!(
            view.accessible_label.contains("Recording on Cloud"),
            "{}",
            view.accessible_label
        );
        assert!(
            view.accessible_label
                .contains("does not apply to this Recording"),
            "{}",
            view.accessible_label
        );
    }

    #[test]
    fn loading_and_unavailable_are_visible_at_idle() {
        let loading = view_from_response(&response(
            DaemonState::Idle,
            Some(asr(AsrMode::Local, None, LocalReadiness::Loading)),
        ));
        assert_eq!(loading.visible_label, "Loading");
        assert!(
            loading
                .accessible_label
                .contains("no deferred capture queue")
        );
        let unavailable = view_from_response(&response(
            DaemonState::Idle,
            Some(asr(
                AsrMode::Local,
                None,
                LocalReadiness::Unavailable {
                    error: "Local selected; model unavailable".into(),
                },
            )),
        ));
        assert_eq!(
            unavailable.visible_label,
            "Local selected; model unavailable"
        );
        assert!(
            unavailable
                .accessible_label
                .contains("refused before capture")
        );
    }

    #[test]
    fn cloud_idle_stays_hidden() {
        let view = view_from_response(&response(
            DaemonState::Idle,
            Some(asr(AsrMode::Cloud, None, LocalReadiness::Absent)),
        ));
        assert_eq!(view, OverlayView::HIDDEN);
    }

    #[test]
    fn key_repeat_does_not_reannounce_the_same_recording() {
        let mut latch = TriggerRepeatLatch::default();
        assert!(latch.observe(Some("rec-1")));
        assert!(!latch.observe(Some("rec-1")));
        assert!(latch.observe(Some("rec-2")));
        assert!(!latch.observe(None));
    }

    #[test]
    fn status_poll_gate_admits_only_one_in_flight() {
        let gate = StatusPollGate::new();
        let first = StatusPollGuard::acquire(&gate);
        assert!(first.is_some());
        assert!(StatusPollGuard::acquire(&gate).is_none());
        drop(first);
        assert!(StatusPollGuard::acquire(&gate).is_some());
    }

    #[test]
    fn screen_reader_uses_the_accessible_label() {
        let view = OverlayView::daemon_unavailable();
        assert_eq!(screen_reader_announcement(view), view.accessible_label);
        assert!(daemon_stall_announcement().contains("cannot reach voisu-daemon"));
    }
}
