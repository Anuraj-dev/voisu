# Voisu — State
> Cloud-first Linux desktop dictation app (Fedora KDE Plasma / Wayland) · Last checkpoint: 2026-07-16

## 🚧 In progress / next
- Ticket 13 round-1 review fixes are complete on `ticket-13-fedora-package`; next is the orchestrator's Fedora host
  run of the exact RPM artifact and the live checks listed in `docs/release-evidence.md`.
- `rpmbuild` is unavailable in this managed sandbox, so the RPM build script/spec are headlessly reviewed and the
  actual package, install, upgrade, removal, portal, Recording, Delivery, and clipboard checks remain PENDING.

## Status
- `voisu` and `voisu-daemon` communicate over bounded, versioned Unix IPC; the actor keeps status responsive while
  Recording capture, provider completion, reconciliation, validation, and Delivery run behind owned boundaries.
- PipeWire capture streams one-second Deepgram chunks and bounded overlapping Groq WAV chunks concurrently under one
  Provider Deadline. Cancellation owns, kills, reaps, and awaits every child before Idle.
- The Transcript pipeline deterministically selects near agreement, reconciles material disagreement, applies
  Unicode-aware guardrails, permits one bounded repair, and otherwise falls back to a clean Source Transcript or
  reports a Quality Failure. Only the final Transcript reaches Delivery.
- Ticket 09 installs a graphical-session-owned daemon service with atomic binaries and a three-starts-per-30-seconds
  failure bound; packaged `/usr/lib/systemd/user/voisu.service` is preferred over and migrates away old XDG
  user-data ownership only after validating `/usr/bin/voisu-daemon` and the unit's `ExecStart`; invalid packaged
  metadata clearly falls back to the Ticket 09 user-data path; daemon lifecycle remains independent from the optional Overlay.
- Ticket 12 keeps the Overlay observer-only: a pure, headless selection layer chooses runtime-advertised Layer Shell,
  an unfocusable regular GTK surface, desktop notification, or a persistent journal observer when no display exists.
  Structured Overlay logs and `voisu-overlay --report-backend` expose `backend` plus `degradation`; normal `voisu
  status` remains daemon-only. A missing dynamic GTK runtime fails before `main` and is recorded by the launching
  service/journal rather than falsely selected as an Overlay backend. `voisu-overlay --supervise` bounds separate
  Overlay restarts to three failures in 30 seconds and never touches the daemon.
- Ticket 12 round-2 review fixes are in: the Overlay's surface probe is now honest local GTK realization (no unsound
  compositor-map timer, no false fallback on a healthy compositor), and the capsule stays hidden at Idle with no startup
  flash while status polling starts immediately.
- Current gates: `cargo test --workspace` — 202 passed, 2 ignored, 0 failed;
  `cargo check -p voisu-app --features overlay`, `cargo build --workspace`, `bash -n` for packaging scripts, and
  `git diff --check` are clean. Offline vendor/archive construction passed; `rpmbuild` and RPM/live Fedora
  evidence are pending the host.

## Architecture map
- Domain, IPC, lifecycle/Delivery evidence, provider coordination, decision pipeline -> `crates/voisu-core/src/lib.rs`
- Fedora adapters: PipeWire, providers, clipboard, zbus portals, native libei -> `crates/voisu-app/src/system.rs`
- systemd user-service installation, lifecycle, ownership/IPC reporting -> `crates/voisu-app/src/service.rs`
- Fedora RPM spec, exact-commit build, and smoke harness -> `packaging/`; install/upgrade/removal procedure ->
  `docs/packaging-fedora.md`
- Release evidence matrix and host checklist -> `docs/release-evidence.md`
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
- The Fedora release uses one GTK-free base RPM plus an optional Overlay subpackage; `Cargo.lock`, an exact-commit
  vendor archive, and `--offline` bind the tested source to a reproducible RPM build.
- RPM removal follows desktop-user `voisu service uninstall` before `dnf remove`, because per-user systemd
  scriptlets cannot reliably clear a running unit or enablement under `~/.config`.

## Gotchas
- Use `CONTEXT.md` terms exactly; several ordinary synonyms are banned.
- Default workspace builds are GTK-free; compile the optional Overlay with
  `cargo check -p voisu-app --features overlay`.
- This managed sandbox denies Unix-domain and private D-Bus socket binds with `EPERM`; run socket-heavy acceptance on
  the host/orchestrator.
- `rustfmt` and `clippy` are unavailable (`cargo fmt` is not installed).
