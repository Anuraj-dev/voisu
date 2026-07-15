# Voisu — State
> Cloud-first Linux desktop dictation app (Fedora KDE Plasma / Wayland) · Last checkpoint: 2026-07-15

## 🚧 In progress / next
- Ticket 04 first-review fixes remain uncommitted. Two parallel-suite acceptance flakes now have load-tolerant fixtures;
  the orchestrator must run `cargo test --workspace` and three consecutive parallel `cargo test -p voisu-app` runs,
  then run the Sol re-review at medium effort before commit/close/push and the model-benchmark update.

## Status
- The independent `voisu` and `voisu-daemon` binaries communicate over bounded, versioned Unix IPC.
- Public `voisu doctor`, `voisu auth set`, and `voisu auth verify` cover Fedora readiness and Secret-Service-backed
  provider credentials with explicit environment fallback for development/headless use.
- Normal daemon startup captures the configured/default PipeWire microphone as 16 kHz mono s16 PCM and concurrently
  sends one-second live PCM chunks to Deepgram plus bounded 30-second overlapping WAV chunks to Groq.
- Deepgram queues request tasks behind a three-permit cap, preserving audio ingestion and ordered transcript assembly
  without allowing a long Recording to fan out into hundreds of curl processes.
- Stop finalizes the audio tail and accepts valid Source Transcripts within the shared Provider Deadline; a deadline
  loser is cancelled, killed, reaped, and awaited inside the recovery budget before completion can publish `Idle`.
- Provider completion is deterministic and exactly-once; completion futures retain spawned request handles until each
  await finishes, so a deadline loser remains owned for cancellation, kill, reap, and awaited cleanup before `Idle`.
  Structured IPC evidence reports first chunk, capture finalization, per-provider completion, accepted providers,
  release-to-text timing, and Delivery count.
- Capture/provider failure and capture EOF return the daemon to idle; Deepgram and Groq cancellation use the
  per-Recording `CancelRegistry`, owning-child kill/reap, and awaited request-task cleanup before reuse.
- The uncommitted Ticket 04 fix worktree retains the awaited late-provider cleanup semantics. Its acceptance fixtures
  now gate provider-start failure on the capture PID marker, avoid CPU spin loops, and use a two-second test-only
  Provider Deadline for the late-curl reap assertion; the orchestrator's parallel flake gate is pending.

## Architecture map
- Domain, audio contract, provider coordination/timings, typed errors, readiness/auth traits, IPC ->
  `crates/voisu-core/src/lib.rs`
- Lifecycle actor, secure socket ownership, Recording pump, Provider Deadline evidence, controlled test adapters ->
  `crates/voisu-app/src/bin/voisu-daemon.rs`
- Thin public CLI and command-specific bounded response waits -> `crates/voisu-app/src/bin/voisu.rs`
- Hardened PipeWire, Deepgram/Groq HTTP, clipboard, readiness, Secret Service, and process adapters ->
  `crates/voisu-app/src/system.rs`
- Public daemon/CLI acceptance suite, PATH stubs, local Groq server, live smoke ->
  `crates/voisu-app/tests/daemon_cli_lifecycle.rs`
- Provider coordination contract tests -> `crates/voisu-core/tests/provider_coordination.rs`
- Ordered implementation tickets -> `.scratch/voisu-implementation/issues/`

## Stack & run
- Stack: Rust 2024 + Tokio + serde · Run: `cargo run -p voisu-app --bin voisu-daemon` · Test: `cargo test --workspace`

## Key decisions (see docs/decisions.md and docs/adr/)
- One actor serializes lifecycle transitions while spawned work keeps status responsive during boundary work.
- Credentials are stdin-only, Secret-Service-backed values; every subprocess clears its environment and has bounded,
  capped, owning-handle cleanup. Curl is always `-q` first and receives credentials only through stdin configuration.
- Deepgram receives one-second non-overlapping linear16 chunks through at most three concurrent curl owners; queued
  results are concatenated in chunk order, while Groq retains overlap-removal for its overlapping audio chunks.
- The 15-second Provider Deadline is shared across both completions; valid sources are attributed and sorted, and one
  available Source Transcript proceeds only after any late provider's bounded awaited cleanup completes.
- Recovery remains a first-class actor state; cancellation is an `AtomicBool` observed by the wait loop owning `Child`,
  never a raw-PID signal.

## Gotchas
- Use CONTEXT.md's ubiquitous language exactly; it lists banned synonyms.
- `rustfmt` and `clippy` are unavailable (`sudo dnf install rustfmt clippy` needed); both remain skipped.
- The sandbox exposes `.git` read-only; keep the review fixes uncommitted until a writable Git environment is available.
