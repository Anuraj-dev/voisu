# ADR: GTK4 locked in, Electron rejected

**Label:** `wayfinder:task` (AFK, small)
**Status:** open
**Blocked by:** —
**Blocks:** —

## Question

Write the ADR in `docs/adr/` recording the toolkit decision: GTK4 +
gtk4-layer-shell stays; Electron rejected (no native layer-shell path in
Chromium/Ozone, 150–250 MB Chromium + CVE re-shipping treadmill per package,
every comparable tool uses GTK+layer-shell); Tauri is the only acceptable
web-tech fallback if a dashboard is ever wanted. Evidence: research digest §1
and benchmark rows 122/133. Driver-inline or Luna low — it's a doc.
