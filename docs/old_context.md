# Old context — Voisu
> ⚠️ Reconstructed from the codebase, README, and planning docs when the context system was adopted on
> 2026-07-15. This is a best-effort summary of what the project is and how far it had progressed BEFORE
> per-session tracking began. It is NOT a record of exact past sessions or decisions — those weren't
> captured. Treat specifics as inferred, not authoritative.

## What this project is
Voisu is a cloud-first Linux desktop dictation application targeting Fedora KDE Plasma on Wayland.
Press the Trigger Key, speak, press again, and a validated Transcript is delivered to the focused
application (clipboard fallback). Transcription runs through Groq and Deepgram concurrently and is
reconciled with quality guardrails.

## How far it had progressed
Planning only — no source code and no git commits. Completed planning artifacts:
- Domain language / ubiquitous language (`CONTEXT.md`)
- Linux platform research (`docs/research/linux-platform.md`)
- Six ADRs (`docs/adr/0001`–`0006`)
- Wayfinder planning map (`.scratch/voisu-wayfinder/map.md`)
- Approved specification (`.scratch/voisu-spec/issues/01-fedora-cloud-dictation.md`)
- Ten ordered implementation tickets (`.scratch/voisu-implementation/issues/01`–`10`)

## Notable structure / entry points
No code entry points yet. Key docs: `CONTEXT.md`, `docs/adr/`, `.scratch/voisu-implementation/issues/`.

## Inferred stack & tooling
Rust (planned, per ADR-0002); GTK4 for the optional Overlay; PipeWire audio; XDG portals + libei;
systemd user service; RPM/DEB packaging planned. Nothing scaffolded yet.
