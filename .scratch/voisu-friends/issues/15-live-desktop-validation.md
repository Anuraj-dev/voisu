# Live desktop validation: Hyprland EIS smoke + KDE + GNOME passes

**Label:** `wayfinder:task` (HITL — needs real/VM Wayland sessions)
**Status:** open
**Blocked by:** 14-release-workflow-ci-smoke
**Blocks:** —

## Question

The part CI cannot test. Per desktop, a scripted checklist Raja (or a friend,
guided) runs once on a VM or real session, installing from the real channel:

1. **Hyprland (Omarchy VM or friend's machine)** — THE open risk: mainline
   xdg-desktop-portal-hyprland's RemoteDesktop/EIS path is under-documented
   (research digest §2); validate auto-type end-to-end, note whether the
   third-party portal add-on is needed; overlay layer-shell; clipboard.
2. **KDE Plasma (non-Fedora distro or the Fedora host as baseline)** — full
   pass: doctor, dictation, overlay, auto-type, `voisu delivery guarded`.
3. **Ubuntu GNOME (VM)** — install from apt repo, plain-window fallback
   renders, clipboard delivery works, auto-type after the manual
   Settings → Remote Desktop enable (capture the exact steps for the
   onboarding docs fog item).

Resolution records the per-desktop results and graduates the fog items
(Ubuntu min-version claim, GNOME first-run UX, onboarding docs) into tickets
or docs. Friends install only after this closes.
