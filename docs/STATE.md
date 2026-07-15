# Voisu — State
> Cloud-first Linux desktop dictation app (Fedora KDE Plasma / Wayland) · Last checkpoint: 2026-07-15

## 🚧 In progress / next
- Ticket 04 is implemented and fully verified but uncommitted. Run the required first Sol review at high effort, resolve
  any findings, then commit `feat(dictation): stream Deepgram within the Provider Deadline (#4)` and close issue #4.

## Status
- The independent `voisu` and `voisu-daemon` binaries communicate over bounded, versioned Unix IPC.
- Public `voisu doctor`, `voisu auth set`, and `voisu auth verify` cover Fedora readiness and Secret-Service-backed
  provider credentials with explicit environment fallback for development/headless use.
- Normal daemon startup captures the configured/default PipeWire microphone as 16 kHz mono s16 PCM and concurrently
  sends one-second live PCM chunks to Deepgram plus bounded 30-second overlapping WAV chunks to Groq.
- Stop finalizes the audio tail, accepts both valid Source Transcripts within the shared Provider Deadline, or proceeds
  with the one valid Source Transcript available when the other provider fails or is late.
- Provider completion is deterministic and exactly-once; structured IPC evidence reports first chunk, capture
  finalization, per-provider completion, accepted providers, release-to-text timing, and Delivery count.
- Capture/provider failure and capture EOF return the daemon to idle; Deepgram and Groq cancellation use the
  per-Recording `CancelRegistry`, owning-child kill/reap, and awaited request-task cleanup before reuse.
- `cargo build --workspace`, `cargo test --workspace`, and `git diff --check` pass: 71 tests green plus one ignored,
  opt-in live Fedora microphone/Groq/clipboard smoke test. The `voisu-app` suite passed three consecutive full runs.

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
- Deepgram receives one-second linear16 HTTP chunks during the Recording through the shared hardened curl boundary;
  Groq retains 30-second WAV chunks with 500 ms overlap and the final tail.
- The 15-second Provider Deadline is shared across both completions; valid sources are attributed and sorted, and one
  available Source Transcript proceeds without allowing late work to create another Delivery.
- Recovery remains a first-class actor state; cancellation is an `AtomicBool` observed by the wait loop owning `Child`,
  never a raw-PID signal.

## Gotchas
- Use CONTEXT.md's ubiquitous language exactly; it lists banned synonyms.
- `rustfmt` and `clippy` are unavailable (`sudo dnf install rustfmt clippy` needed); both remain skipped.
- The sandbox exposes `.git` read-only, so do not attempt staging or commits here.
- The required first Ticket 04 review still needs a writable Codex state directory; never deliberately background it.
