# GNOME plain-window overlay fallback (runtime layer-shell detection)

**Label:** `wayfinder:task` (AFK, implementation)
**Status:** open
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
