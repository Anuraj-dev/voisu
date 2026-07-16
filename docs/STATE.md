# Voisu — State
> Cloud-first Linux desktop dictation app (Fedora KDE Plasma / Wayland) · Last checkpoint: 2026-07-16

## 🚧 In progress / next
- Ticket 08 (issue #8, libei Delivery through the RemoteDesktop portal) is review-complete locally. Sol first review
  found protocol, compatibility, acknowledgement, persistence, and re-prompt defects; all were fixed. The requested
  Sol re-review was unavailable because its account hit the usage cap, so the fixes received a focused manual
  re-review against the official APIs. Next: publish the branch, require exact-head green CI, and merge.
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
- Ticket 08 adds clipboard-first Delivery: `wl-copy` must preserve the final Transcript before compositor submission.
  The daemon reports `compositor_submitted` or `clipboard_fallback` plus a sanitized fallback reason in IPC/history.
- Production RemoteDesktop setup runs in the background, requests keyboard permission with `persist_mode=2`, then
  follows CreateSession -> SelectDevices -> Start -> ConnectToEIS on one persistent zbus connection. A five-second
  libei sender budget covers UTF-8 text, frame, and pong acknowledgement; TEXT capability makes Unicode independent
  of the active keyboard layout. Denial, revocation, unavailable capability, disconnect, and rejection retain the
  clipboard and report explicit fallback. Failed/revoked sessions are never reused.
- Native libei is loaded by SONAME at runtime, so building does not require `libei-devel`; libei 1.6 TEXT is preferred,
  while 1.5 uses the active EIS XKB keymap to submit Ctrl+V for the preserved clipboard. Standard acceptance daemons set `VOISU_DISABLE_DIRECT_DELIVERY=1` unless they
  inject a private bus, so tests never prompt the host desktop; the opt-in live smoke keeps compositor Delivery enabled.
- Current inventory: 153 tests listed (7 app unit + 100 daemon/CLI acceptance + 6 Delivery + 20 diagnostics +
  6 provider-coordination + 14 Transcript-decision). The full host gate is green: 152 passed, 1 opt-in live smoke
  ignored, 0 failed.

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
- Only a validated final Transcript crosses the Delivery boundary, and clipboard preservation gates compositor submission.
- RemoteDesktop permission setup is background work so a desktop dialog never extends stop-to-Delivery latency;
  pending or failed setup immediately falls back to the already-preserved clipboard.
- zbus portal sessions retain the caller identity that created them; libei uses TEXT when advertised and otherwise
  resolves Ctrl+V from the active EIS XKB keymap. A pong confirms compositor processing, never application acceptance.

## Gotchas
- Use `CONTEXT.md` terms exactly; several ordinary synonyms are banned.
- `rustfmt` and `clippy` are unavailable (`cargo fmt` is not installed).
- This managed sandbox denies Unix-domain and private D-Bus socket binds with `EPERM`; compile-check socket tests and
  run their acceptance gate on the orchestrator/host.
- The installed Fedora libei here is 1.5 and lacks TEXT symbols introduced in 1.6; the keyboard path therefore needs
  the EIS device to provide an XKB keymap, while a missing keymap fails closed to the preserved clipboard.
- Claude delegation was attempted twice as mandated but its backend returned `API Error: Unable to connect to API
  (ENOTIMP)`; the implementation used narrowly scoped local reads instead.
