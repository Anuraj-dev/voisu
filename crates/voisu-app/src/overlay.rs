//! Presentation-only state derived from the daemon's public status response.
//! This module owns no Recording, provider, or Delivery work.

use std::time::Duration;

use voisu_core::{DaemonState, Response};

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
    pub accessible_label: &'static str,
}

impl OverlayView {
    pub const HIDDEN: Self = Self {
        phase: OverlayPhase::Hidden,
        activity: 0,
        accessible_label: "",
    };

    pub fn from_response(response: &Response) -> Self {
        if !response.ok {
            return Self::failure();
        }
        match response.state {
            Some(DaemonState::Recording) => Self {
                phase: OverlayPhase::Recording,
                activity: response
                    .evidence
                    .as_ref()
                    .map(|evidence| (evidence.streamed_chunk_count.min(3)) as u8)
                    .unwrap_or(1),
                accessible_label: "Recording; voice activity visible",
            },
            Some(DaemonState::Processing) => Self {
                phase: OverlayPhase::Processing,
                activity: 0,
                accessible_label: "Processing Recording",
            },
            Some(DaemonState::Idle) | None => Self::HIDDEN,
        }
    }

    pub const fn success() -> Self {
        Self {
            phase: OverlayPhase::Success,
            activity: 0,
            accessible_label: "Delivered",
        }
    }

    pub const fn failure() -> Self {
        Self {
            phase: OverlayPhase::Failure,
            activity: 0,
            accessible_label: "Quality Failure",
        }
    }

    pub const fn is_visible(self) -> bool {
        !matches!(self.phase, OverlayPhase::Hidden)
    }

    pub const fn animation_interval(self, reduced_motion: bool) -> Option<Duration> {
        if reduced_motion || !matches!(self.phase, OverlayPhase::Recording) {
            None
        } else {
            Some(Duration::from_millis(160))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use voisu_core::{DaemonState, Response};

    #[test]
    fn idle_is_hidden_and_has_no_animation() {
        let view = OverlayView::from_response(&Response::success(DaemonState::Idle, "idle"));
        assert_eq!(view, OverlayView::HIDDEN);
        assert_eq!(view.animation_interval(false), None);
    }

    #[test]
    fn phases_are_accessible_and_motion_is_reduced() {
        let recording = OverlayView::from_response(&Response::success(
            DaemonState::Recording,
            "Recording",
        ));
        assert_eq!(recording.phase, OverlayPhase::Recording);
        assert!(recording.is_visible());
        assert_eq!(recording.animation_interval(true), None);
        assert_eq!(OverlayView::from_response(&Response::rejected(None, "offline")).phase, OverlayPhase::Failure);
        assert_eq!(OverlayView::success().accessible_label, "Delivered");
    }
}
