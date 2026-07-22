//! Headless capability selection for the optional Overlay feedback surface.
//!
//! This intentionally contains no GTK calls. The `voisu-overlay` binary owns
//! probing GTK and creating a surface; contract tests inject the resulting
//! capabilities here without needing a compositor or display.

use std::collections::VecDeque;
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FeedbackBackend {
    /// Rung 1: a GTK4 Layer Shell surface (Wayland compositors that advertise
    /// zwlr_layer_shell_v1).
    LayerShell,
    /// Rung 3: an `org.freedesktop.Notifications` desktop notification. Needs
    /// nothing installed and works on Cinnamon, GNOME, KDE, and XFCE. Rung 2 (a
    /// plain GTK4 window) is deliberately skipped — see `select_feedback_backend`.
    DesktopNotification,
    /// Rung 4: journal-only, when no display exists at all.
    JournalLog,
}

impl FeedbackBackend {
    pub const fn label(self) -> &'static str {
        match self {
            Self::LayerShell => "layer-shell",
            Self::DesktopNotification => "desktop-notification",
            Self::JournalLog => "journal-log",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FeedbackDegradation {
    X11,
    XwaylandFallback,
    MissingDisplay,
    LayerShellUnavailable,
    SurfaceCreationFailure,
    UnknownSession,
}

impl FeedbackDegradation {
    pub const fn label(self) -> &'static str {
        match self {
            Self::X11 => "x11",
            Self::XwaylandFallback => "xwayland-fallback",
            Self::MissingDisplay => "missing-display",
            Self::LayerShellUnavailable => "layer-shell-unavailable",
            Self::SurfaceCreationFailure => "surface-creation-failure",
            Self::UnknownSession => "unknown-session",
        }
    }
}

// The display-session kind is detected once, in `voisu-core`, so the clipboard
// backend, the feedback ladder here, and `doctor` all agree. Re-exported so the
// overlay binary keeps importing it from this module.
pub use voisu_core::SessionKind;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FeedbackCapabilities {
    pub session: SessionKind,
    pub display_available: bool,
    /// A Wayland session has no usable Wayland display but can present via X11.
    pub xwayland_fallback: bool,
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
    if !capabilities.display_available {
        return FeedbackSelection {
            backend: FeedbackBackend::JournalLog,
            degradation: Some(FeedbackDegradation::MissingDisplay),
        };
    }
    match capabilities.session {
        SessionKind::Wayland if capabilities.layer_shell_supported => FeedbackSelection {
            backend: FeedbackBackend::LayerShell,
            degradation: None,
        },
        // Rung 2 (a plain GTK4 window) is deliberately skipped: it needs GTK4,
        // which the X11/Cinnamon target lacks, and would not save that host
        // anyway. Every display session without a Layer Shell surface drops
        // straight to the desktop-notification rung, which needs nothing
        // installed and renders on Cinnamon, GNOME, KDE, and XFCE.
        SessionKind::Wayland => FeedbackSelection {
            backend: FeedbackBackend::DesktopNotification,
            degradation: Some(FeedbackDegradation::LayerShellUnavailable),
        },
        SessionKind::X11 => FeedbackSelection {
            backend: FeedbackBackend::DesktopNotification,
            degradation: Some(if capabilities.xwayland_fallback {
                FeedbackDegradation::XwaylandFallback
            } else {
                FeedbackDegradation::X11
            }),
        },
        SessionKind::Unknown => FeedbackSelection {
            backend: FeedbackBackend::DesktopNotification,
            degradation: Some(FeedbackDegradation::UnknownSession),
        },
    }
}

/// A GTK surface can still fail to be created locally after a viable backend
/// was selected. `surface_created` reflects local GTK realization
/// (`window.surface().is_some()` on the first real show) — the only surface
/// failure the observer can honestly detect in-process. A compositor that
/// *rejects* the surface (e.g. a Layer Shell protocol error) does not surface
/// here: it raises a Wayland protocol error that terminates the process, and
/// the bounded `voisu-overlay --supervise` policy — not a false in-process
/// timer — converts that into explicit degraded behavior. On genuine local
/// failure, fall back to a desktop notification rather than retrying the daemon
/// or silently losing feedback.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_display_uses_a_persistent_journal_observer_instead_of_a_noop_notification() {
        let selection = select_feedback_backend(FeedbackCapabilities {
            session: SessionKind::Wayland,
            display_available: false,
            xwayland_fallback: false,
            layer_shell_supported: true,
        });

        assert_eq!(selection.backend, FeedbackBackend::JournalLog);
        assert_eq!(selection.degradation, Some(FeedbackDegradation::MissingDisplay));
    }

    #[test]
    fn a_realized_surface_keeps_its_backend_and_only_genuine_absence_falls_back() {
        // Honesty contract (round-2 finding 1): `after_surface_creation`'s flag
        // is LOCAL GTK realization (`window.surface().is_some()`), not a
        // compositor map confirmation. A realized surface must never be
        // downgraded, so a false desktop-notification fallback on a healthy
        // compositor is impossible. Only a genuinely absent surface object
        // degrades to a notification. A compositor that *rejects* the surface
        // raises a Wayland protocol error that terminates the process; the
        // bounded --supervise policy (exercised by
        // `red_bounded_overlay_restarts_stop_without_a_daemon_control_path`)
        // converts that into explicit degraded behavior. Only a live compositor
        // can prove that process-termination half of the story.
        let layer = select_feedback_backend(FeedbackCapabilities {
            session: SessionKind::Wayland,
            display_available: true,
            xwayland_fallback: false,
            layer_shell_supported: true,
        });
        assert_eq!(layer.backend, FeedbackBackend::LayerShell);
        // A created (realized) surface preserves the selected backend verbatim.
        assert_eq!(after_surface_creation(layer, true), layer);
        // Only genuine local absence degrades to a desktop notification.
        let fallback = after_surface_creation(layer, false);
        assert_eq!(fallback.backend, FeedbackBackend::DesktopNotification);
        assert_eq!(fallback.degradation, Some(FeedbackDegradation::SurfaceCreationFailure));
    }

    #[test]
    fn available_x11_display_is_a_named_xwayland_fallback_when_wayland_is_absent() {
        let selection = select_feedback_backend(FeedbackCapabilities {
            session: SessionKind::X11,
            display_available: true,
            xwayland_fallback: true,
            layer_shell_supported: false,
        });

        // Rung 2 is skipped: an X11/XWayland session goes to the notification
        // rung, still naming the XWayland fallback as the cause.
        assert_eq!(selection.backend, FeedbackBackend::DesktopNotification);
        assert_eq!(selection.degradation, Some(FeedbackDegradation::XwaylandFallback));
    }

    #[test]
    fn a_plain_x11_session_uses_the_notification_rung_not_a_gtk_window() {
        let selection = select_feedback_backend(FeedbackCapabilities {
            session: SessionKind::X11,
            display_available: true,
            xwayland_fallback: false,
            layer_shell_supported: false,
        });
        assert_eq!(selection.backend, FeedbackBackend::DesktopNotification);
        assert_eq!(selection.degradation, Some(FeedbackDegradation::X11));
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
