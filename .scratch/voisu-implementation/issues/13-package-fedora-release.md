# 13 — Package and verify the Fedora release candidate

**Parent:** [Build reliable cloud-first dictation for Fedora Wayland](../../voisu-spec/issues/01-fedora-cloud-dictation.md)

**What to build:** A reproducible Fedora package containing the exact tested
daemon, CLI, service integration, portal metadata, and Overlay, with evidence
that install-to-dictation works on the target desktop.

**Blocked by:** 10 — Recover cleanly from real workflow failures; 12 — Fall back when Layer Shell is unavailable.

**Status:** ready-for-agent

- [ ] The package installs only declared runtime dependencies and pinned Voisu artifacts.
- [ ] Fresh install, login start, setup, real Recording, direct Delivery, and clipboard fallback pass on Fedora KDE Wayland.
- [ ] Upgrade preserves supported configuration and credentials without retaining stale executables or service paths.
- [ ] Removal disables the service and removes packaged artifacts while preserving user data unless explicitly purged.
- [ ] The exact packaged build passes the standard suite and opt-in Fedora smoke suite.
- [ ] Release evidence includes process ownership, portal behavior, provider fallback, latency spans, log redaction, and Overlay isolation.
- [ ] APT/DEB work remains a subsequent milestone rather than silently expanding this Fedora release.

