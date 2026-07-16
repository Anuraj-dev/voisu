# 08 — Deliver text through libei with clipboard fallback

**Parent:** [Build reliable cloud-first dictation for Fedora Wayland](../../voisu-spec/issues/01-fedora-cloud-dictation.md)

**What to build:** Compositor-authorized Delivery of the final Transcript to the
focused application, with a guaranteed clipboard path whenever direct input is
not possible.

**Blocked by:** 02 — Verify Fedora readiness and store cloud credentials safely; 05 — Reconcile and guard the final Transcript.

**Status:** ready-for-agent

- [ ] Setup requests persistent keyboard-emulation permission through the desktop portal where supported.
- [ ] A final Transcript is submitted to the Fedora KDE compositor through libei; the public result does not claim the focused application accepted it because libei exposes no application-level acknowledgement.
- [ ] Unicode, punctuation, multiline text, and active keyboard layouts are covered by observable Delivery tests.
- [ ] Clipboard preservation succeeds before compositor submission is reported.
- [ ] Denial, revocation, unavailable usable input capability, disconnection, and compositor rejection produce explicit clipboard fallback.
- [ ] Partial or candidate text is never sent to the focused application.
