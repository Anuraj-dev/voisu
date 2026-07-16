# 12 — Fall back when Layer Shell is unavailable

**Parent:** [Build reliable cloud-first dictation for Fedora Wayland](../../voisu-spec/issues/01-fedora-cloud-dictation.md)

**What to build:** Feedback that remains useful when Layer Shell, Wayland, or a
graphical display is unavailable, without allowing the Overlay to crash-loop the
daemon.

**Blocked by:** 11 — Show daemon state in a separate GTK4 voice capsule.

**Status:** complete

- [x] Runtime capability detection selects Layer Shell only when the compositor advertises support.
- [x] A regular unfocusable GTK surface or desktop notification reports essential states when Layer Shell is unavailable.
- [x] X11/XWayland and missing display produce explicit degraded behavior; a local surface-creation failure (GTK realizes without a surface) falls back to a desktop notification, while a compositor that rejects the surface terminates the process and is bounded by the Overlay `--supervise` policy; a missing GTK runtime is an ELF loader failure recorded by the launching service/journal, not a self-reported Overlay backend.
- [x] Repeated Overlay failure uses bounded restart policy and never restarts or terminates the daemon.
- [x] CLI status and logs identify the selected feedback backend and degradation reason.
- [x] Contract tests cover selection and failure behavior without requiring every compositor in CI.
