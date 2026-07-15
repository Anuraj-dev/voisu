# 07 — Toggle Recording through the Global Shortcuts portal

**Parent:** [Build reliable cloud-first dictation for Fedora Wayland](../../voisu-spec/issues/01-fedora-cloud-dictation.md)

**What to build:** A user-approved Fedora KDE Trigger Key whose successive
activations start and stop the same reliable Recording workflow without raw
keyboard access.

**Blocked by:** 02 — Verify Fedora readiness and store cloud credentials safely; 05 — Reconcile and guard the final Transcript.

**Status:** ready-for-agent

- [ ] Setup requests and displays a desktop-approved Trigger Key binding.
- [ ] The first activation starts one Recording and the next activation stops it.
- [ ] Repeated or concurrent activation signals cannot create overlapping Recordings or duplicated stop processing.
- [ ] The Recording Deadline automatically stops a forgotten toggle and reports why.
- [ ] Permission denial, revocation, portal restart, and unavailable portal leave CLI start/stop/toggle usable.
- [ ] Standard tests exercise the public portal contract through controlled D-Bus responses.

