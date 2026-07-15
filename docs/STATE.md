# Voisu — State
> Cloud-first Linux desktop dictation app (Fedora KDE Plasma / Wayland) · Last checkpoint: 2026-07-15

## 🚧 In progress / next
- Ticket 03 (PipeWire → Groq → clipboard end-to-end slice, issue #3) is COMPLETE and CLOSED at commit
  `f51dbbd`, after 5 Sol review rounds. Next: dispatch Ticket 04 (concurrent Deepgram streaming + Provider
  Deadline) to codex Sol medium; first review of the ticket runs at Sol high, re-reviews at Sol medium.

## Status
- The independent `voisu` and `voisu-daemon` binaries communicate over bounded, versioned Unix IPC.
- Public `voisu doctor`, `voisu auth set`, and `voisu auth verify` cover Fedora readiness and Secret-Service-backed
  provider credentials with explicit environment fallback for development/headless use.
- Normal daemon startup captures the configured/default PipeWire microphone as 16 kHz mono s16 PCM, submits
  bounded overlapping Groq chunks during the Recording, includes final frames after stop, validates the resulting
  Merge Result, and preserves the final Transcript with `wl-copy` Delivery.
- Empty, too-short, silent, and over-Recording-Deadline outcomes are distinct and recoverable; capture/provider
  failure or capture EOF automatically returns the daemon to idle for the next Recording without an explicit Stop.
- Lifecycle now has an explicit `ActorState::Recovering(u64)`: Start/Toggle requests during recovery get a
  retryable rejection instead of being queued (queuing risked ordering violations); a `Recovered(id)` ack gates
  the return to Idle. `abort_recording_work` runs capture abort and provider-coordinator abort concurrently
  (`tokio::join!`) within a 2s `RECOVERY_ABORT_DEADLINE`, itself inside the 22s `PROCESSING_RESPONSE_DEADLINE`.
- `CancelRegistry` no longer sends raw-PID `SIGKILL` (that had a PID-reuse race — Sol HIGH finding). It's now an
  `AtomicBool` cancel flag; the bounded-wait loop that owns each `Child` kills it via its own handle on a ~10ms
  poll tick, and already-cancelled operations fail fast without spawning.
- Groq permits plaintext HTTP only for loopback development endpoints; production failure coverage uses real PATH
  subprocess stubs and local HTTP for validation, 5xx, Provider Deadline, capture death, and missing `wl-copy`.
- Production Deepgram remains an explicit unavailable stream until Ticket 04; the existing coordinator accepts the
  valid Groq Source Transcript without waiting for an unimplemented provider.
- `cargo build --workspace` and `cargo test --workspace` pass: 65 tests green (2 system unit + 58 daemon/CLI
  acceptance + 5 provider-coordination), confirmed 3x flake-free.
- Review policy: first Sol review of a ticket runs at high effort; every re-review until merge runs at medium
  effort (recorded in `AGENTS.md`/`CLAUDE.md`).

## Architecture map
- Domain, audio contract, provider coordination, typed errors, readiness/auth traits, IPC ->
  `crates/voisu-core/src/lib.rs`
- Lifecycle actor (incl. `ActorState::Recovering`, recovery abort/ack), secure socket ownership, Recording pump ->
  `crates/voisu-app/src/bin/voisu-daemon.rs`
- Thin public CLI and command-specific bounded response waits -> `crates/voisu-app/src/bin/voisu.rs`
- Hardened PipeWire, Groq HTTP, clipboard, readiness, Secret Service, process, and `CancelRegistry` adapters ->
  `crates/voisu-app/src/system.rs`
- Public daemon/CLI acceptance suite, local Groq server, PATH stubs, live smoke ->
  `crates/voisu-app/tests/daemon_cli_lifecycle.rs`
- Provider coordination contract tests -> `crates/voisu-core/tests/provider_coordination.rs`
- Ordered implementation tickets -> `.scratch/voisu-implementation/issues/`
- Per-dispatch model benchmark (Sol/Terra/Luna vs Opus) -> `docs/model-benchmark.md`

## Stack & run
- Stack: Rust 2024 + Tokio + serde · Run: `cargo run -p voisu-app --bin voisu-daemon` · Test: `cargo test --workspace`

## Key decisions (see docs/decisions.md and docs/adr/)
- One actor serializes lifecycle transitions while spawned work keeps status responsive during boundary work.
- Credentials are stdin-only, Secret-Service-backed values; environment variables are an explicit non-persistent fallback.
- Every subprocess clears its environment, receives a minimal allowlist, has capped retained streams, and is killed/reaped
  under a whole-operation deadline; curl is always `-q` first and receives credentials through standard input config.
- PipeWire is normalized to 16 kHz mono s16 PCM; graceful SIGINT plus a bounded drain preserves final frames.
- Groq uses 30-second chunks with 500 ms overlap, a 15-second Provider Deadline, deterministic word-overlap merging,
  and exactly one clipboard Delivery after validation.
- Recovery is a first-class actor state with a retryable-rejection policy (not a deferral queue) and a bounded,
  concurrent capture+provider abort under `RECOVERY_ABORT_DEADLINE`.
- Subprocess cancellation is an `AtomicBool` flag checked by the handle-owning waiter, never a raw-PID signal.

## Gotchas
- Use CONTEXT.md's ubiquitous language exactly; it lists banned synonyms.
- `rustfmt` and `clippy` are unavailable (`sudo dnf install rustfmt clippy` needed); both remain skipped.
- Never deliberately background a `codex exec` run.
- If the Codex state directory is ever read-only again, the pinned Sol review command fails to initialize —
  retry in a writable environment rather than trying to work around it.
