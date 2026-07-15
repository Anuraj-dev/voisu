# Voisu — State
> Cloud-first Linux desktop dictation app (Fedora KDE Plasma / Wayland) · Last checkpoint: 2026-07-16

## 🚧 In progress / next
- Ticket 04 (issue #4) is APPROVED and closed. Next up: dispatch Ticket 05 (reconcile + guard Transcript) —
  Sol medium implementation (architectural: reconciliation), first review Sol high per the pinned routing.
- Follow-up issue #14 filed: make `DeepgramStream::abort` / `GroqStream::abort` cancellation-safe
  (drain→peek-then-pop) plus an abort-deadline regression test. Not blocking Ticket 05 but should be picked
  up soon.

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
  Deepgram completion errors also cancel and await every later retained chunk handle before publishing `Idle`.
  Structured IPC evidence reports first chunk, capture finalization, per-provider completion, accepted providers,
  release-to-text timing, and Delivery count.
- Capture/provider failure and capture EOF return the daemon to idle; Deepgram and Groq cancellation use the
  per-Recording `CancelRegistry`, owning-child kill/reap, and awaited request-task cleanup before reuse.
- Linux capture children request `PR_SET_PDEATHSIG(SIGKILL)`. Acceptance daemons run in isolated process groups whose
  Drop guard kills the whole tree, and all generated shell stubs have signal/exit traps plus bounded wait loops.
- CI is live: `.github/workflows/ci.yml` runs the workspace suite plus a 3x-parallel voisu-app flake gate on every
  push/PR; green on all commits pushed so far.
- Test count: 76 (67 voisu-app acceptance + 3 unit + 6 voisu-core), 1 ignored live smoke.

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
- CI workflow -> `.github/workflows/ci.yml`
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
- Routing update (2026-07-16): Opus 4.8 subagents (medium/high effort) are now the workhorse for regular
  implementation/fix work; Sol is reserved for architectural tickets and ALL code reviews (first review high,
  re-reviews medium).

## Gotchas
- Use CONTEXT.md's ubiquitous language exactly; it lists banned synonyms.
- `rustfmt` and `clippy` are unavailable (`sudo dnf install rustfmt clippy` needed); both remain skipped.
- A stale git stash ("partial edits from killed codex leak-fix run") and an older one ("partial review-fix from
  killed codex run") both remain on the stack — superseded by the merged fixes; safe to drop.
