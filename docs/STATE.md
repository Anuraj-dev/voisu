# Voisu — State
> Cloud-first Linux desktop dictation app (Fedora KDE Plasma / Wayland) · Last checkpoint: 2026-07-15

## 🚧 In progress / next
- Ticket 04 (issue #4, concurrent Deepgram + Provider Deadline) feature is committed as `3e4eecc` (71 tests). First
  review (Sol HIGH) returned REQUEST-CHANGES: 2 HIGH (Provider Deadline cancellation not awaited before Idle; no
  in-flight cap on Deepgram curls) + 2 MEDIUM (`VOISU_TEST_DEEPGRAM_UNAVAILABLE` production bypass; wrong
  overlap-removal merge applied to non-overlapping Deepgram chunks).
- Sol's medium fix round produced **UNCOMMITTED worktree changes** claiming all 4 findings fixed (75 tests), BUT the
  new test `provider_deadline_kills_and_reaps_late_deepgram_curl_before_idle`
  (`crates/voisu-app/tests/daemon_cli_lifecycle.rs:2047`) **fails deterministically** (0.14s) on the orchestrator's
  machine: "the late Deepgram curl must be reaped before Idle is observable". 65 other acceptance tests pass. Do
  **not** trust the "fully verified" claim in the prior session log for this round — it was wrong.
  - A Sol medium feedback round is (or was, when this checkpoint was written) IN FLIGHT fixing that root cause via a
    background codex run; it may have modified the worktree further since.
- Next steps: verify the fix (full `cargo test --workspace` green + 3x flake-free `cargo test -p voisu-app`), then
  commit as `fix(dictation): resolve Ticket 04 review findings (#4)`, dispatch a Sol **medium** re-review (re-reviews
  are medium per policy), iterate to APPROVE, close issue #4, push, update `docs/model-benchmark.md` rows for the
  ticket 04 fix rounds, and checkpoint. If Sol fails again, escalate to an Opus subagent (high effort) per the
  fallback ladder.

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
- Provider completion is deterministic and exactly-once; structured IPC evidence reports first chunk, capture
  finalization, per-provider completion, accepted providers, release-to-text timing, and Delivery count.
- Capture/provider failure and capture EOF return the daemon to idle; Deepgram and Groq cancellation use the
  per-Recording `CancelRegistry`, owning-child kill/reap, and awaited request-task cleanup before reuse.
- Last fully-verified, committed state (`3e4eecc`) is 71 tests green plus one ignored, opt-in live Fedora
  microphone/Groq/clipboard smoke test. The uncommitted Ticket 04 fix-round worktree claims 75 tests but has one
  known-failing test (see In progress) and must be reverified before it can be trusted or committed.

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
