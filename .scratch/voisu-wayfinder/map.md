# Map — Define the Fedora-first Voisu implementation plan

**Label:** `wayfinder:map`

## Destination

An approved, implementation-ready specification and tracer-bullet ticket graph
for a reliable cloud-first Voisu daemon on Fedora KDE Wayland, followed by a
lightweight GTK4 Overlay without redesigning the daemon.

## Notes

- Use the `domain-modeling`, `tdd`, `to-spec`, `to-tickets`, and `raja-design`
  skills where their phases apply.
- Fedora KDE Wayland is first; APT-based distribution follows successful Fedora
  verification.
- Plan decisions, not implementation. Implementation is RED -> GREEN ->
  REFACTOR, one public behavior at a time.
- The local Markdown tracker is authoritative until a remote tracker exists.

## Decisions so far

- [Choose the transcription execution model](issues/01-cloud-transcription.md) — cloud-only Groq and Deepgram run concurrently.
- [Choose the Transcript Delivery experience](issues/02-automatic-delivery.md) — automatically insert final text with clipboard fallback.
- [Choose Recording control semantics](issues/03-toggle-recording.md) — first Trigger Key press starts and the next stops.
- [Choose partial versus final Delivery](issues/04-final-only-delivery.md) — only the reconciled and validated final Transcript is delivered.
- [Choose the implementation stack](issues/05-rust-gtk-stack.md) — independent Rust daemon and native GTK4 Overlay.
- [Choose the relationship with HyprVox](issues/06-independent-implementation.md) — architectural inspiration without forking.
- [Choose daemon and Overlay boundaries](issues/07-process-boundary.md) — versioned Unix IPC between separately supervised processes.
- [Choose diagnostic retention posture](issues/08-local-diagnostics.md) — local structured evidence and opt-in expiring audio.
- [Choose provider completion policy](issues/09-provider-deadline.md) — bounded quality wait with valid single-provider fallback.
- [Choose Wayland input integrations](issues/10-portal-integration.md) — desktop portals and libei, without privileged raw input.
- [Choose CLI command language](issues/11-cli-language.md) — Recording and service commands remain unambiguous.
- [Confirm the public TDD seam](issues/12-confirm-public-test-seam.md) — exercise the real daemon through CLI/IPC with replaceable external boundaries.
- [Approve the implementation ticket graph](issues/13-approve-implementation-graph.md) — publish thirteen dependency-ordered tracer bullets without merges or splits.

## Not yet specified

None. Visual tokens are deliberately deferred to the approved
[Show daemon state in a separate GTK4 voice capsule](../voisu-implementation/issues/11-gtk-voice-capsule.md)
prototype after the reliable daemon milestone.

## Out of scope

- Local speech inference and training a speech model.
- Supporting every Linux distribution before Fedora KDE Wayland succeeds.
- Building the GTK4 Overlay before the reliable daemon path.
- Automatic telemetry upload.
- Privileged raw keyboard capture or `/dev/uinput` injection on the normal path.
