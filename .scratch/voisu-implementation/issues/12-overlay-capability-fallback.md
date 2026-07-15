# 12 — Fall back when Layer Shell is unavailable

**Parent:** [Build reliable cloud-first dictation for Fedora Wayland](../../voisu-spec/issues/01-fedora-cloud-dictation.md)

**What to build:** Feedback that remains useful when Layer Shell, Wayland, or a
graphical display is unavailable, without allowing the Overlay to crash-loop the
daemon.

**Blocked by:** 11 — Show daemon state in a separate GTK4 voice capsule.

**Status:** ready-for-agent

- [ ] Runtime capability detection selects Layer Shell only when the compositor advertises support.
- [ ] A regular unfocusable GTK surface or desktop notification reports essential states when Layer Shell is unavailable.
- [ ] X11, missing display, missing GTK dependency, and surface-creation failure produce explicit degraded behavior.
- [ ] Repeated Overlay failure uses bounded restart policy and never restarts or terminates the daemon.
- [ ] CLI status and logs identify the selected feedback backend and degradation reason.
- [ ] Contract tests cover selection and failure behavior without requiring every compositor in CI.

