# Proposed tracer-bullet breakdown

## Parent

[Build reliable cloud-first dictation for Fedora Wayland](../voisu-spec/issues/01-fedora-cloud-dictation.md)

## 1. Prove the daemon lifecycle through the public CLI

**Blocked by:** None.

**What it delivers:** A Cargo workspace containing the real daemon and CLI. In
an isolated test environment, `voisu start` and `voisu stop` drive one complete
Recording through controlled audio, provider, validation, and Delivery
boundaries; `voisu status` observes the versioned Unix IPC state. The first test
is written RED and production code grows only enough to make it GREEN.

## 2. Verify Fedora readiness and store cloud credentials safely

**Blocked by:** Prove the daemon lifecycle through the public CLI.

**What it delivers:** A user can run setup/readiness checks, select or confirm a
microphone, store Groq and Deepgram credentials through desktop secret storage,
and verify provider authentication without beginning a real Recording. Secret
values are redacted from all public output and structured events.

## 3. Dictate through PipeWire and Groq into the clipboard

**Blocked by:** Verify Fedora readiness and store cloud credentials safely.

**What it delivers:** On Fedora, `voisu start` captures a real PipeWire
microphone and `voisu stop` sends the Recording to Groq, validates the Source
Transcript, and places the final Transcript on the clipboard. This is the first
usable cloud dictation path and retains controlled audio/provider substitutes
for the standard test suite.

## 4. Stream concurrently to Deepgram with a Provider Deadline

**Blocked by:** Dictate through PipeWire and Groq into the clipboard.

**What it delivers:** Deepgram receives continuous frames while Groq receives
bounded overlapping chunks. Stop finalizes the tail, accepts reordered
responses, and enforces the Provider Deadline. The user receives a valid
single-provider Transcript when the other provider is slow or unavailable.

## 5. Reconcile and guard the final Transcript

**Blocked by:** Stream concurrently to Deepgram with a Provider Deadline.

**What it delivers:** Near-identical Source Transcripts select deterministically;
material disagreements use bounded cloud reconciliation. Candidate text passes
quality guardrails, one bounded recovery path, and clean-source fallback before
one final Transcript reaches Delivery.

## 6. Inspect and expire correlated local diagnostics

**Blocked by:** Reconcile and guard the final Transcript.

**What it delivers:** Every Recording exposes a correlation ID connecting audio
timing, chunks, provider results, reconciliation, validation, and Delivery.
Users can inspect history, export redacted evidence, enable expiring debug audio,
and verify retention cleanup without automatic telemetry upload.

## 7. Toggle Recording through the Global Shortcuts portal

**Blocked by:** Reconcile and guard the final Transcript; Verify Fedora
readiness and store cloud credentials safely.

**What it delivers:** A Fedora KDE user approves a Trigger Key through the XDG
portal. One activation starts a Recording and the next stops it. A Recording
Deadline handles forgotten toggles, and CLI bindings remain usable when the
portal is unavailable or permission is denied.

## 8. Deliver text through libei with clipboard fallback

**Blocked by:** Reconcile and guard the final Transcript; Verify Fedora
readiness and store cloud credentials safely.

**What it delivers:** A Fedora KDE user grants portal permission and receives
the final Transcript directly in the focused application. Unicode Delivery,
denial, revocation, unsupported capability, and application rejection all
produce an observable result while preserving the Transcript on the clipboard.

## 9. Own the daemon through a systemd user service

**Blocked by:** Dictate through PipeWire and Groq into the clipboard.

**What it delivers:** `voisu service install|start|stop|restart|status|remove`
manages one idempotent systemd user service that starts after login, uses current
XDG runtime state, avoids stale display variables and duplicate daemon owners,
and survives safe upgrades.

## 10. Recover cleanly from real workflow failures

**Blocked by:** Inspect and expire correlated local diagnostics; Toggle Recording
through the Global Shortcuts portal; Deliver text through libei with clipboard
fallback; Own the daemon through a systemd user service.

**What it delivers:** Forced microphone loss, provider disconnects, malformed
responses, Provider Deadline expiry, portal revocation, CLI crashes, daemon
restart, and abrupt shutdown leave the next Recording usable. Fedora smoke tests
exercise real devices and providers only when explicitly enabled.

## 11. Show daemon state in a separate GTK4 voice capsule

**Blocked by:** Recover cleanly from real workflow failures.

**What it delivers:** A separately supervised GTK4 process consumes the
versioned state stream and shows the approved compact Recording, processing,
success, and failure states on KDE Layer Shell. Killing or restarting it cannot
interrupt dictation. Visual work follows `DESIGN.md`, reduced-motion rules, and
the screenshot-critique gate.

## 12. Fall back when Layer Shell is unavailable

**Blocked by:** Show daemon state in a separate GTK4 voice capsule.

**What it delivers:** Capability detection selects Layer Shell only where it is
supported and uses a regular GTK surface or desktop notification elsewhere. A
missing display or failed Overlay never crash-loops the daemon.

## 13. Package and verify the Fedora release candidate

**Blocked by:** Fall back when Layer Shell is unavailable; Recover cleanly from
real workflow failures.

**What it delivers:** A reproducible Fedora package installs the exact tested
daemon, CLI, service integration, portal metadata, and Overlay. Installation,
login restart, upgrade, removal, real dictation, direct Delivery, fallback, log
redaction, and process ownership are verified against the packaged build.

