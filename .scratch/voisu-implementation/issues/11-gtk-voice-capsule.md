# 11 — Show daemon state in a separate GTK4 voice capsule

**Parent:** [Build reliable cloud-first dictation for Fedora Wayland](../../voisu-spec/issues/01-fedora-cloud-dictation.md)

**What to build:** A separately supervised, lightweight GTK4 Overlay that
observes the stable daemon state stream without owning any dictation work.

**Blocked by:** 10 — Recover cleanly from real workflow failures.

**Status:** ready-for-agent

- [ ] `DESIGN.md` locks the approved visual tokens before UI implementation begins.
- [ ] The Overlay is hidden and performs no animation work while the daemon is idle.
- [ ] Recording shows restrained voice activity; processing, success, and failure have distinct accessible states.
- [ ] The surface cannot take keyboard focus or interfere with the focused application.
- [ ] Killing, disconnecting, or restarting the Overlay cannot interrupt a Recording or Delivery.
- [ ] Reduced motion and contrast requirements pass the design review gate.
- [ ] Rendered Fedora screenshots are critiqued against `DESIGN.md`, corrected, and captured again before completion.

