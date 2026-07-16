# Voisu — State
> Cloud-first Linux desktop dictation app (Fedora KDE Plasma / Wayland) · Last checkpoint: 2026-07-16

## 🚧 In progress / next
- Ticket 11 implementation is complete on `ticket-11-voice-capsule`; next: host/orchestrator must compile the opt-in GTK feature and complete the Fedora KDE screenshot gate.
- Ticket 10 merged through PR #17 at `aa8055a` (squash); exact-head CI passed and issue #10 is closed.
  Reviews: Sol high round 1 (2 BLOCKER + 1 HIGH + 1 MEDIUM), fixes in `0865286`, Sol medium round 2 APPROVE.
- Issue #14 review round 1 is fixed on `issue-14-cancellation-safe-abort`: abort-deadline drop now actively cancels
  and retains each adapter task through a reaper, while provider cancellation preserves curl child reap. The
  deterministic regression is proven RED with `drain(..)` reinstated. Next: Sol medium re-review, PR, exact-head
  CI, merge, and close #14.
- After #14: Ticket 11 (issue #11, GTK4 voice capsule) starts the overlay phase — routing note: Luna per the
  benchmark plan (Luna unused so far). Benchmark log current through Ticket 10 (`docs/model-benchmark.md`).

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
- Ticket 09 adds `voisu service install|start|stop|restart|status|uninstall`. Installation atomically replaces a
  trusted daemon copy under XDG user data and enables one unit under `graphical-session.target`, ordered after the
  user D-Bus socket, PipeWire, and desktop portal without baking session variables or checkout paths. Service reports
  combine real systemd state with daemon IPC state; manual ownership and duplicate races fail without restart loops.
- Ticket 10 proves next-Recording recovery across real PipeWire process death/reconnection, provider disconnect/
  malformed/quota/deadline fallback, portal revocation/restart with clipboard Delivery, killed CLI and daemon
  processes, repeated failure, stale socket takeover, and owned boundary-process reap. Persistent service failures
  are limited to three starts in 30 seconds. A separate ignored Fedora smoke covers microphone, both providers,
  portals, systemd interruption/restart, Delivery, and the next Recording.
- Every daemon/CLI external child now receives a guarded Linux parent-death signal through the shared spawn path;
  the child refuses exec if its owner died during the fork/exec window. The user-service CLI applies the same guard
  to `systemctl`. Deterministic probes turn RED when either production call is removed.
- Portal revocation/restart acceptance now runs real PipeWire/provider/clipboard adapters against a private portal
  bus and asserts exact clipboard bytes. The live Fedora recovery smoke refuses an existing Voisu installation,
  verifies the daemon PID changes after interruption, and disables/removes its debug service even after panic.
- Current inventory: 173 tests listed (11 app unit + 106 daemon/CLI acceptance + 6 Delivery + 10 user-service +
  20 diagnostics + 6 provider-coordination + 14 Transcript-decision). The full host gate is green: 171 passed,
  2 opt-in live smokes ignored, 0 failed; `cargo build --workspace` is clean.
- Ticket 11 review round is fixed on `ticket-11-voice-capsule`: OverlayStatus now carries typed, ID-versioned terminal events; normal CLI Status remains unchanged; the presentation controller consumes terminal events once and expires them; GTK input-region, token, glyph, disconnect, and accessibility treatments are implemented behind the opt-in feature. Full default workspace tests/build are green. GTK feature compilation and real Fedora screenshots remain pending host verification because this sandbox lacks GTK development libraries and a desktop surface.

## Architecture map
- Domain, IPC, lifecycle/Delivery evidence, provider coordination, decision pipeline -> `crates/voisu-core/src/lib.rs`
- Local diagnostic record/store/export/replay -> `crates/voisu-core/src/diagnostics.rs`
- Fedora adapters: PipeWire, providers, clipboard, zbus portals, native libei -> `crates/voisu-app/src/system.rs`
- systemd user-service installation, lifecycle, ownership/IPC reporting -> `crates/voisu-app/src/service.rs`
- Lifecycle actor, cancellation ownership, Delivery response/evidence -> `crates/voisu-app/src/bin/voisu-daemon.rs`
- Public CLI -> `crates/voisu-app/src/bin/voisu.rs`
- Daemon/CLI and private portal-bus acceptance -> `crates/voisu-app/tests/daemon_cli_lifecycle.rs`
- Delivery boundary and RemoteDesktop/libei acceptance -> `crates/voisu-app/tests/delivery.rs`
- User-service public CLI acceptance -> `crates/voisu-app/tests/service_cli.rs`
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
- The daemon service is enabled by the graphical session, inherits that session's current environment, and orders
  after D-Bus, PipeWire, and the desktop portal; no checkout or volatile session value is embedded in the unit.
- Repeated service startup failures are rate-limited by systemd; workflow failures remain Recording-scoped and
  never cross the exactly-once Delivery boundary.
- Every spawned external process is configured to die with its owning process, with a parent-race check before exec.

## Gotchas
- Use `CONTEXT.md` terms exactly; several ordinary synonyms are banned.
- `rustfmt` and `clippy` are unavailable (`cargo fmt` is not installed).
- This managed sandbox denies Unix-domain and private D-Bus socket binds with `EPERM`; compile-check socket tests and
  run their acceptance gate on the orchestrator/host.
- The installed Fedora libei here is 1.5 and lacks TEXT symbols introduced in 1.6; the keyboard path therefore needs
  the EIS device to provide an XKB keymap, while a missing keymap fails closed to the preserved clipboard.
- External review workers may be unavailable even after their quota resets: Ticket 09's final Sonnet reader produced
  no output, and both Ticket 10 Sonnet attempts also returned no usable review; do not wait on them as a gate.
- The Ticket 10 full Fedora recovery smoke is intentionally unrun in standard gates; it requires explicit
  `VOISU_LIVE_RECOVERY_SMOKE=1` plus a real Fedora desktop, credentials, portals, and systemd user session.
