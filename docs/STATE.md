# Voisu — State
> Cloud-first Linux desktop dictation app (Fedora KDE Plasma / Wayland) · Last checkpoint: 2026-07-16

## 🚧 In progress / next
- Ticket 08 (issue #8, libei Delivery through the RemoteDesktop portal) is implemented but uncommitted. Next:
  run the full 148-test host gate (the managed sandbox denies Unix/D-Bus socket binds), then Sol first review at
  high effort and fix/re-review until approved. Do not commit in this implementation session.
- Follow-up issue #14 remains open: make Deepgram/Groq abort cancellation-safe plus its regression test; out of
  Ticket 08 scope.

## Status
- `voisu` and `voisu-daemon` communicate over bounded, versioned Unix IPC; the actor keeps status responsive while
  Recording capture, provider completion, reconciliation, validation, and Delivery run behind owned boundaries.
- PipeWire capture streams one-second Deepgram chunks and bounded overlapping Groq WAV chunks concurrently under
  one Provider Deadline. Cancellation owns, kills, reaps, and awaits every child before Idle.
- The Transcript pipeline deterministically selects near agreement, reconciles material disagreement, applies
  Unicode-aware guardrails, permits one bounded repair, and otherwise falls back to a clean Source Transcript or
  reports a Quality Failure. Only the final Transcript reaches Delivery.
- Correlated local diagnostics expose history/export/replay with redaction, bounded retention, private descriptor-
  checked filesystem access, and opt-in expiring debug audio.
- Ticket 07 provides a persistent zbus Global Shortcuts session with revocation/restart handling. Portal acceptance
  tests use a private per-test `dbus-daemon`, never the host desktop.
- Ticket 08 adds clipboard-first Delivery: `wl-copy` must preserve the final Transcript before direct Delivery can
  succeed. The daemon reports `direct` or `clipboard_fallback` plus a sanitized fallback reason in IPC/history.
- Production RemoteDesktop setup runs in the background, requests keyboard permission with `persist_mode=2`, then
  follows CreateSession -> SelectDevices -> Start -> ConnectToEIS on one persistent zbus connection. A five-second
  libei sender budget covers UTF-8 text, frame, and pong acknowledgement; TEXT capability makes Unicode independent
  of the active keyboard layout. Denial, revocation, unavailable capability, disconnect, and rejection retain the
  clipboard and report explicit fallback. Failed/revoked sessions are never reused.
- Native libei is loaded by SONAME at runtime, so building does not require `libei-devel`; missing libei 1.6 TEXT
  symbols fail closed to clipboard. Standard acceptance daemons set `VOISU_DISABLE_DIRECT_DELIVERY=1` unless they
  inject a private bus, so tests never prompt the host desktop; the opt-in live smoke keeps direct Delivery enabled.
- Current inventory: 148 tests listed (3 app unit + 100 daemon/CLI acceptance + 5 Delivery + 20 diagnostics +
  6 provider-coordination + 14 Transcript-decision). Non-socket Delivery/core tests are green; all tests compile.

## Architecture map
- Domain, IPC, lifecycle/Delivery evidence, provider coordination, decision pipeline -> `crates/voisu-core/src/lib.rs`
- Local diagnostic record/store/export/replay -> `crates/voisu-core/src/diagnostics.rs`
- Fedora adapters: PipeWire, providers, clipboard, zbus portals, native libei -> `crates/voisu-app/src/system.rs`
- Lifecycle actor, cancellation ownership, Delivery response/evidence -> `crates/voisu-app/src/bin/voisu-daemon.rs`
- Public CLI -> `crates/voisu-app/src/bin/voisu.rs`
- Daemon/CLI and private portal-bus acceptance -> `crates/voisu-app/tests/daemon_cli_lifecycle.rs`
- Delivery boundary and RemoteDesktop/libei acceptance -> `crates/voisu-app/tests/delivery.rs`
- Ordered implementation tickets -> `.scratch/voisu-implementation/issues/`

## Stack & run
- Stack: Rust 2024 + Tokio + serde + zbus 5 + runtime libei · Run: `cargo run -p voisu-app --bin voisu-daemon`
  · Test: `cargo test --workspace`

## Key decisions (see docs/decisions.md and docs/adr/)
- Portals are the only normal Fedora path for global shortcuts and input emulation; no raw devices or `uinput`.
- All cancellable process/future work follows own -> cancel -> bounded await/reap; no Idle with detached work.
- Only a validated final Transcript crosses the Delivery boundary, and clipboard preservation gates direct success.
- RemoteDesktop permission setup is background work so a desktop dialog never extends stop-to-Delivery latency;
  pending or failed setup immediately falls back to the already-preserved clipboard.
- zbus portal sessions retain the caller identity that created them; libei sends layout-independent UTF-8 only when
  EIS advertises TEXT, otherwise Delivery fails closed to clipboard.

## Gotchas
- Use `CONTEXT.md` terms exactly; several ordinary synonyms are banned.
- `rustfmt` and `clippy` are unavailable (`cargo fmt` is not installed).
- This managed sandbox denies Unix-domain and private D-Bus socket binds with `EPERM`; compile-check socket tests and
  run their acceptance gate on the orchestrator/host.
- The installed Fedora libei here is 1.5 and lacks TEXT symbols introduced in 1.6, so local production direct
  Delivery intentionally falls back; the controlled boundary covers direct UTF-8 behavior and the host/live gate
  must validate a compositor/libei stack that advertises TEXT.
- Claude delegation was attempted twice as mandated but its backend returned `API Error: Unable to connect to API
  (ENOTIMP)`; the implementation used narrowly scoped local reads instead.
