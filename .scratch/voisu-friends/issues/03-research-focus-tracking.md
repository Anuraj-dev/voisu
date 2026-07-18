# Research: focused-window identity tracking on KDE + Hyprland Wayland

**Label:** `wayfinder:research` (AFK)
**Status:** resolved (2026-07-18) — asset: assets/03-focus-tracking-research.md
**Blocked by:** —
**Blocks:** 04-guarded-delivery-mode

## Question

Guarded delivery needs: capture focused-window identity at dictation start,
compare at delivery time. How, per compositor?

- KDE Plasma: KWin scripting API? `org.kde.KWin` D-Bus? plasma-window-management
  protocol? What identity is stable (uuid, resourceClass+caption)?
- Hyprland: `hyprctl activewindow -j` / Hyprland IPC socket — stability, cost.
- Wayland-native options: ext-foreign-toplevel-list / wlr-foreign-toplevel
  (which compositors expose them to regular clients?).
- GNOME: note the answer for later but do NOT design for it (guarded is
  KDE-first per decision; GNOME can degrade to plain type/clipboard).

Deliverable: markdown asset comparing mechanisms (stability of identity across
title changes, permission requirements, subscription vs poll), with a
recommended abstraction seam for `system.rs` (trait like FocusProbe with
per-compositor adapters, detect at runtime). Sonnet 5 scout, web + local code
read-only.
