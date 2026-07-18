# Keep the GTK4 + layer-shell Overlay; reject Electron

The Overlay stays GTK4 with gtk4-layer-shell. Electron is rejected because
Chromium/Ozone has no native wlr-layer-shell path, so an Electron overlay
cannot be an always-on-top Wayland surface at all — only XWayland
override-redirect side effects approximate it — and shipping Chromium adds
150–250 MB plus a CVE re-shipping treadmill to every package we distribute.
Every comparable Wayland dictation/overlay tool (Handy, whisper-overlay,
hyprwhspr) landed on GTK + layer-shell for the same reason.

If a web-tech surface is ever genuinely wanted (for example a settings
dashboard), Tauri is the only acceptable fallback — system WebView, no
bundled Chromium — and it still does not replace the layer-shell Overlay.

Evidence: 2026-07-18 research digest §1 (fact-checked adversarially), model
benchmark rows 122 and 133.
