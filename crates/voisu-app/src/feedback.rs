//! Headless capability selection for the optional Overlay feedback surface.
//!
//! This intentionally contains no GTK calls. The `voisu-overlay` binary owns
//! probing GTK and creating a surface; contract tests inject the resulting
//! capabilities here without needing a compositor or display.

use std::collections::VecDeque;
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FeedbackBackend {
    LayerShell,
    RegularSurface,
    DesktopNotification,
}

impl FeedbackBackend {
    pub const fn label(self) -> &'static str {
        match self {
            Self::LayerShell => "layer-shell",
            Self::RegularSurface => "regular-surface",
            Self::DesktopNotification => "desktop-notification",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FeedbackDegradation {
    X11,
    MissingDisplay,
    MissingGtkDependency,
    LayerShellUnavailable,
    SurfaceCreationFailure,
    UnknownSession,
}

impl FeedbackDegradation {
    pub const fn label(self) -> &'static str {
        match self {
            Self::X11 => "x11",
            Self::MissingDisplay => "missing-display",
            Self::MissingGtkDependency => "missing-gtk-dependency",
            Self::LayerShellUnavailable => "layer-shell-unavailable",
            Self::SurfaceCreationFailure => "surface-creation-failure",
            Self::UnknownSession => "unknown-session",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GtkAvailability {
    Available,
    MissingDependency,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionKind {
    Wayland,
    X11,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FeedbackCapabilities {
    pub session: SessionKind,
    pub display_available: bool,
    pub gtk: GtkAvailability,
    pub layer_shell_supported: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FeedbackSelection {
    pub backend: FeedbackBackend,
    pub degradation: Option<FeedbackDegradation>,
}

impl FeedbackSelection {
    /// Stable structured text for stderr/journal logs and `--report-backend`.
    pub fn report_line(self) -> String {
        let degradation = self.degradation.map(FeedbackDegradation::label).unwrap_or("none");
        format!(
            "overlay_feedback backend={} degradation={degradation}",
            self.backend.label(),
        )
    }
}

/// Select the least-degraded feedback backend from injected runtime facts.
/// Layer Shell is deliberately a runtime capability, never a Cargo target
/// assumption: a Wayland compositor must advertise it before it is selected.
pub const fn select_feedback_backend(capabilities: FeedbackCapabilities) -> FeedbackSelection {
    if matches!(capabilities.gtk, GtkAvailability::MissingDependency) {
        return FeedbackSelection {
            backend: FeedbackBackend::DesktopNotification,
            degradation: Some(FeedbackDegradation::MissingGtkDependency),
        };
    }
    if !capabilities.display_available {
        return FeedbackSelection {
            backend: FeedbackBackend::DesktopNotification,
            degradation: Some(FeedbackDegradation::MissingDisplay),
        };
    }
    match capabilities.session {
        SessionKind::Wayland if capabilities.layer_shell_supported => FeedbackSelection {
            backend: FeedbackBackend::LayerShell,
            degradation: None,
        },
        SessionKind::Wayland => FeedbackSelection {
            backend: FeedbackBackend::RegularSurface,
            degradation: Some(FeedbackDegradation::LayerShellUnavailable),
        },
        SessionKind::X11 => FeedbackSelection {
            backend: FeedbackBackend::RegularSurface,
            degradation: Some(FeedbackDegradation::X11),
        },
        SessionKind::Unknown => FeedbackSelection {
            backend: FeedbackBackend::RegularSurface,
            degradation: Some(FeedbackDegradation::UnknownSession),
        },
    }
}

/// A GTK surface can still fail after a viable backend was selected. Preserve
/// the reason and fall back to a desktop notification rather than retrying the
/// daemon or silently losing feedback.
pub const fn after_surface_creation(
    selection: FeedbackSelection,
    surface_created: bool,
) -> FeedbackSelection {
    if surface_created {
        selection
    } else {
        FeedbackSelection {
            backend: FeedbackBackend::DesktopNotification,
            degradation: Some(FeedbackDegradation::SurfaceCreationFailure),
        }
    }
}

pub const OVERLAY_RESTART_LIMIT: usize = 3;
pub const OVERLAY_RESTART_WINDOW: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OverlayRestartDecision {
    Restart,
    Stop,
}

impl OverlayRestartDecision {
    pub const fn should_restart(self) -> bool {
        matches!(self, Self::Restart)
    }
}

/// Bounded supervision state for the separate Overlay process. It has no
/// daemon dependency, so an Overlay crash cannot restart or terminate the
/// daemon that owns Recording and Delivery.
#[derive(Debug, Default)]
pub struct OverlayRestartPolicy {
    failures: VecDeque<Duration>,
}

impl OverlayRestartPolicy {
    pub fn record_failure(&mut self, now: Duration) -> OverlayRestartDecision {
        let earliest = now.checked_sub(OVERLAY_RESTART_WINDOW).unwrap_or(Duration::ZERO);
        while self.failures.front().is_some_and(|failure| *failure < earliest) {
            self.failures.pop_front();
        }
        self.failures.push_back(now);
        if self.failures.len() < OVERLAY_RESTART_LIMIT {
            OverlayRestartDecision::Restart
        } else {
            OverlayRestartDecision::Stop
        }
    }
}
