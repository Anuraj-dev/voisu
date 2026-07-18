# GNOME plain-window overlay fallback (runtime layer-shell detection)

**Label:** `wayfinder:task` (AFK, implementation)
**Status:** resolved (2026-07-19, PR #59 merged)
**Blocked by:** 02-delivery-mode-enum
**Blocks:** 09-packaging-accounts-setup (phase gate)

## Question

Make the overlay binary work on compositors without zwlr_layer_shell_v1
(GNOME/Mutter): detect layer-shell availability at runtime
(gtk4-layer-shell's is_supported check); when absent, fall back to a small
frameless non-resizable regular GTK4 window, corner-positioned best-effort,
re-`present()`ed on state changes (recording start/stop) so it resurfaces even
though Wayland forbids programmatic keep-above. Clipboard on GNOME must use
GTK/wl_data_device APIs only (never shell-out to wl-copy — also Flatpak-proofs
it; verify current implementation, fix if it shells out). Desktop notification
on recording start as secondary signal. Evidence: research digest §10.
Routing: Terra high; needs a GNOME VM/live session for visual confirmation
(HITL assist or VM screenshot).

## Resolution (2026-07-19)

Implemented by Opus 4.8 (high), reviewed by Sol (2 rounds). Merged as PR #59.

- Runtime layer-shell detection already existed (gtk4_layer_shell::is_supported ->
  FeedbackBackend::RegularSurface); this ticket added the missing fallback behaviors.
- Pure poll_tick seam in overlay.rs: TickAction { Break, Continue { resurface, notify } };
  resurface = once per rendered transition into a visible phase (present() re-raises the
  plain window; Wayland forbids keep-above); notify = RecordingNotifyLatch on OBSERVED
  daemon states (unreachable blips never refire; reachable non-Recording re-arms).
- Surface-handoff guard: a tick that retires the window Breaks before tracker/latch
  mutation — regression-tested.
- Clipboard verification: overlay does none; daemon wl-copy shell-out speaks
  wl_data_device and works on GNOME — Flatpak-proofing deferred to phase B.
- Suites 381/0 both feature sets; overlay build clean.
- Outstanding HITL (non-gating): live GNOME session/VM visual confirmation before the
  friend rollout.
