# Voisu — State
> Cloud-first Linux desktop dictation app (Fedora KDE Plasma / Wayland) · Last checkpoint: 2026-07-16

## 🚧 In progress / next
- Ticket 12 is implemented on `ticket-12-overlay-fallback`; next: review and merge this focused branch, then take
  Ticket 13 (Fedora package verification).
- Issue #14 (cancellation-safe provider abort) is merged through PR #18 at `8d8dbba` and closed; Ticket 11
  (GTK4 voice capsule + completed Fedora screenshot gate) is merged through PR #19 at `66f8aa1` and closed.

## Status
- `voisu` and `voisu-daemon` communicate over bounded, versioned Unix IPC; the actor keeps status responsive while
  Recording capture, provider completion, reconciliation, validation, and Delivery run behind owned boundaries.
- PipeWire capture streams one-second Deepgram chunks and bounded overlapping Groq WAV chunks concurrently under one
  Provider Deadline. Cancellation owns, kills, reaps, and awaits every child before Idle.
- The Transcript pipeline deterministically selects near agreement, reconciles material disagreement, applies
  Unicode-aware guardrails, permits one bounded repair, and otherwise falls back to a clean Source Transcript or
  reports a Quality Failure. Only the final Transcript reaches Delivery.
- Ticket 09 installs a graphical-session-owned daemon service with atomic binaries and a three-starts-per-30-seconds
  failure bound; daemon lifecycle remains independent from the optional Overlay.
- Ticket 12 keeps the Overlay observer-only: a pure, headless selection layer chooses runtime-advertised Layer Shell,
  an unfocusable regular GTK surface, or desktop-notification feedback. Structured Overlay logs and
  `voisu-overlay --report-backend` expose `backend` plus `degradation`; normal `voisu status` remains daemon-only.
  `voisu-overlay --supervise` bounds separate Overlay restarts to three failures in 30 seconds and never touches the
  daemon.
- Current gates: `cargo test --workspace` — 194 passed, 2 ignored, 0 failed;
  `cargo check -p voisu-app --features overlay` and `cargo build --workspace` are clean.

## Architecture map
- Domain, IPC, lifecycle/Delivery evidence, provider coordination, decision pipeline -> `crates/voisu-core/src/lib.rs`
- Fedora adapters: PipeWire, providers, clipboard, zbus portals, native libei -> `crates/voisu-app/src/system.rs`
- systemd user-service installation, lifecycle, ownership/IPC reporting -> `crates/voisu-app/src/service.rs`
- Headless Overlay backend selection and restart policy -> `crates/voisu-app/src/feedback.rs`
- Overlay presentation controller -> `crates/voisu-app/src/overlay.rs`
- GTK Overlay runtime adapter and observer-only status polling -> `crates/voisu-app/src/bin/voisu-overlay.rs`
- Lifecycle actor -> `crates/voisu-app/src/bin/voisu-daemon.rs`
- Public CLI -> `crates/voisu-app/src/bin/voisu.rs`
- Ordered implementation tickets -> `.scratch/voisu-implementation/issues/`

## Stack & run
- Stack: Rust 2024 + Tokio + serde + zbus 5 + GTK4 (opt-in) + runtime libei · Run:
  `cargo run -p voisu-app --bin voisu-daemon` · Test: `cargo test --workspace`

## Key decisions (top 3–5)
- Portals are the only normal Fedora path for global shortcuts and input emulation; no raw devices or `uinput`.
- Only a validated final Transcript crosses the Delivery boundary, and clipboard preservation gates compositor submission.
- The daemon service is graphical-session owned and rate-limits only persistent startup failure; Recording failures
  never replay Delivery.
- Overlay presentation is observer-only and may disappear; Layer Shell is a runtime compositor capability, with
  separate regular-surface/notification feedback and a bounded Overlay-only supervisor.
- Every spawned external process receives a guarded Linux parent-death signal.

## Gotchas
- Use `CONTEXT.md` terms exactly; several ordinary synonyms are banned.
- Default workspace builds are GTK-free; compile the optional Overlay with
  `cargo check -p voisu-app --features overlay`.
- This managed sandbox denies Unix-domain and private D-Bus socket binds with `EPERM`; run socket-heavy acceptance on
  the host/orchestrator.
- `rustfmt` and `clippy` are unavailable (`cargo fmt` is not installed).
