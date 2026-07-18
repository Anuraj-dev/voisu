# One-pass daemon hygiene sweep (bundled minors from the audit)

**Label:** `wayfinder:task` (AFK, implementation — bundled minors)
**Status:** open
**Blocked by:** EXTERNAL — the latency effort completing (these files are
latency-contended; sweep runs after latency integrates to main)
**Blocks:** —

## Question

Single low-effort pass (one Luna/Terra session, one review) over the audit's
minor findings — all mechanical, no behavior change:

1. Replace the `unreachable!()` state backstops at
   `voisu-daemon.rs:566, 883, 971` with `let-else`/match returning a rejected
   `Response` instead of a live panic surface.
2. Extract the 3× duplicated "take validator/delivery, spawn
   process_recording" block (`voisu-daemon.rs:575-586, 891-902, 979-991` —
   line numbers will have shifted after tickets 01/02 and latency; re-locate)
   into one helper, and revisit the `take().expect(...)` adapter pattern while
   there.
3. Extract the 3× duplicated `close_portal_session` +
   `classify_remote_desktop_failure` triplet in
   `FedoraRemoteDesktopPortal::connect` (`system.rs:3157-3257`) into a
   `fail_and_close` helper.
4. Newtype the payloads of `Command::Export`/`Command::Replay`
   (`voisu-core/lib.rs:76,79`) so a correlation ID and a fixture path can't be
   swapped at a call site. Wire-format compatibility must hold (serde transparent).
5. Replace the bare 50 ms sleeps in `daemon_cli_lifecycle.rs:272, 350, 787`
   with the deadline-poll pattern already used elsewhere in that file.
6. Fold in any non-trivial clippy findings deferred from ticket 04.

All 297+ tests stay green; no public behavior changes. Skip anything that turns
out non-mechanical — note it in the resolution instead of expanding scope.
Evidence: [audit report](../assets/audit-2026-07-18.md), minor findings.
