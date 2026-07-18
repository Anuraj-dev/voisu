# Move the blocking pw-record stop path off the Tokio worker threads

**Label:** `wayfinder:task` (AFK, implementation — CRITICAL audit fix C2)
**Status:** open
**Blocked by:** EXTERNAL — `feature/transcription-accuracy` integrating to main.
Frontier the moment that merges; lands BEFORE any latency ticket.
**Blocks:** — (latency effort informally; see STATE.md priority line)

## Question

`PipeWireActiveCapture::finish` and `abort`
(`crates/voisu-app/src/system.rs:1476, 1488`) call `stop_child`
(`system.rs:1358-1417`) directly inside `Box::pin(async move { … })`.
`stop_child`'s `wait_for_child` (`:1202-1232`) and two `bounded_join`s
(`:1143-1160`) busy-poll with `thread::sleep(PROCESS_POLL)` (10 ms) for up to
`PROCESS_DEADLINE` (2 s) each — so every Recording stop/abort can block a Tokio
worker thread synchronously for seconds, starving status queries, other
connections, and the shortcut listener.

Fix: run `stop_child` via `tokio::task::spawn_blocking`, the same pattern the
file already uses correctly for curl (`system.rs:647`) and libei FFI
(`system.rs:3271-3277`). Watch the ownership shape: `stop_child` takes
`&mut self` and the async blocks own `self` — restructure minimally (e.g. move
the taken `child`/reader handles into the blocking closure) rather than
redesigning the capture type. Behavior must not change: same error
classification (`interrupted_cleanly` logic, `system.rs:1400-1415`), same
SIGINT-graceful vs SIGKILL semantics.

Verification: existing capture lifecycle tests stay green; add/extend a test
asserting the daemon answers a status query promptly while a stop with a
slow-exiting fake `pw-record` is in flight, if the harness allows it cheaply.
Evidence and full context: [audit report](../assets/audit-2026-07-18.md),
finding C2.
