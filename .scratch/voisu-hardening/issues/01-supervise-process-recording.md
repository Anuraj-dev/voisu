# Supervise process_recording so a capture-pump panic cannot wedge the daemon

**Label:** `wayfinder:task` (AFK, implementation — CRITICAL audit fix C1)
**Status:** open
**Blocked by:** EXTERNAL — `feature/transcription-accuracy` integrating to main.
Frontier the moment that merges; lands BEFORE any latency ticket.
**Blocks:** — (latency effort informally; see STATE.md priority line)

## Question

Eliminate the only known way to permanently wedge the daemon.
`crates/voisu-app/src/bin/voisu-daemon.rs:1396` does
`pump.await.expect("capture pump should not panic")` inside
`process_recording`, which is bare-`tokio::spawn`ed at `:577, :893, :982` with
no supervisor. If the spawned `capture_pump` task (`:1316-1370`) panics — e.g.
via any poisoned `std::sync::Mutex` in `PipeWireActiveCapture`
(`system.rs:1284,1288,1298,1352,1456,1478` all `.lock().unwrap()`) — the panic
is swallowed by Tokio, `ActorMessage::Completed` is never sent, and
`actor_loop` stays in `ActorState::Processing` forever, rejecting every
Start/Stop/Toggle until daemon restart.

The codebase already contains the correct pattern for the sibling replay path:
`supervise_replay` (`voisu-daemon.rs:498-505`, with the comment at `:485-488`
naming exactly this failure mode). Mirror it: supervise the
`process_recording` task (and treat a `JoinError` from `pump.await` as a failed
Recording, not a panic), so any panic converts into a Completed-with-failure
message and the actor returns to `Idle` with a diagnostic record — preserving
the no-silent-absence discipline (`account_for_missing_providers`).

TDD through the public seam: a test in `daemon_cli_lifecycle.rs` that forces a
pump panic (via the `VOISU_TEST_MODE` controlled capture) and asserts the
daemon answers a subsequent `voisu start` instead of rejecting it. Evidence and
full context: [audit report](../assets/audit-2026-07-18.md), finding C1.
