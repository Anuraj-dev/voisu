# Voisu — State
> Cloud-first Linux desktop dictation app (Fedora KDE Plasma / Wayland) · Last checkpoint: 2026-07-15

## 🚧 In progress / next
- Planning is complete and approved; **no code exists yet**. Next: begin implementation with ticket
  `.scratch/voisu-implementation/issues/01-prove-daemon-cli-lifecycle.md`.

## Status
- Zero commits, zero source files — this is a planning-complete, pre-implementation repo.
- Done: domain language (CONTEXT.md), platform research, 6 ADRs, approved spec, 10 implementation tickets.

## Architecture map (planned, not built)
- Rust daemon (core: Recording → dual cloud transcription → reconcile → Delivery)
- Separate optional GTK4 Overlay over a versioned Unix socket (build ONLY after daemon milestone)
- Providers: Groq (bounded overlapping chunks) + Deepgram (continuous stream), bounded Provider Deadline
- Input/output via XDG portals: Global Shortcuts (Trigger Key), Remote Desktop + libei (Delivery); clipboard fallback

## Stack & run
- Stack: Rust (planned; no Cargo.toml yet) · Run: TODO · Test: TODO (RED → GREEN → REFACTOR cycles required)

## Key decisions (see docs/adr/ for full text)
- Cloud-only dual-provider transcription (Groq + Deepgram), no local model (ADR-0001)
- Independent Rust codebase, not a HyprVox fork (ADR-0002)
- Daemon and Overlay are separate processes (ADR-0003)
- XDG portals only — never raw input devices or privileged uinput (ADR-0004)
- Concurrent streaming with bounded quality wait (ADR-0005)
- Diagnostics local-only; raw audio opt-in and auto-expiring (ADR-0006)

## Gotchas
- Use CONTEXT.md's ubiquitous language exactly (Recording, Transcript, Trigger Key, Delivery, …) — it lists banned synonyms.
- Implementation must follow the approved spec `.scratch/voisu-spec/issues/01-fedora-cloud-dictation.md` and ticket order.
- Preserve MIT attribution for anything adapted from HyprVox.
