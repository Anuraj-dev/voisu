# Voisu — State
> Cloud-first Linux desktop dictation app (Fedora KDE Plasma / Wayland) · Last checkpoint: 2026-07-15

## 🚧 In progress / next
- Ticket 03 is implemented and verified. Retry the pinned independent Sol review when the Codex state directory is
  writable, then commit `feat(dictation): deliver PipeWire Recordings through Groq (#3)` and dispatch Ticket 04.

## Status
- The independent `voisu` and `voisu-daemon` binaries communicate over bounded, versioned Unix IPC.
- Public `voisu doctor`, `voisu auth set`, and `voisu auth verify` cover Fedora readiness and Secret-Service-backed
  provider credentials with explicit environment fallback for development/headless use.
- Normal daemon startup now captures the configured/default PipeWire microphone as 16 kHz mono s16 PCM, submits
  bounded overlapping Groq chunks during the Recording, includes final frames after stop, validates the resulting
  Merge Result, and preserves the final Transcript with `wl-copy` Delivery.
- Empty, too-short, silent, and over-Recording-Deadline outcomes are distinct and recoverable; capture/provider
  failure returns the daemon to idle for the next Recording.
- Production Deepgram remains an explicit unavailable stream until Ticket 04; the existing coordinator accepts the
  valid Groq Source Transcript without waiting for an unimplemented provider.
- `cargo build --workspace`, `cargo test --workspace`, and `git diff --check` pass: 51 tests green plus one ignored,
  opt-in live Fedora microphone/Groq/clipboard smoke test.

## Architecture map
- Domain, audio contract, provider coordination, typed errors, readiness/auth traits, IPC ->
  `crates/voisu-core/src/lib.rs`
- Lifecycle actor, secure socket ownership, Recording pump, controlled test adapters ->
  `crates/voisu-app/src/bin/voisu-daemon.rs`
- Thin public CLI and command-specific bounded response waits -> `crates/voisu-app/src/bin/voisu.rs`
- Hardened PipeWire, Groq HTTP, clipboard, readiness, Secret Service, and process adapters ->
  `crates/voisu-app/src/system.rs`
- Public daemon/CLI acceptance suite, local Groq server, PATH stubs, live smoke ->
  `crates/voisu-app/tests/daemon_cli_lifecycle.rs`
- Provider coordination contract tests -> `crates/voisu-core/tests/provider_coordination.rs`
- Ordered implementation tickets -> `.scratch/voisu-implementation/issues/`

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

## Gotchas
- Use CONTEXT.md's ubiquitous language exactly; it lists banned synonyms.
- `rustfmt` and `clippy` are unavailable (`sudo dnf install rustfmt clippy` needed); both remain skipped.
- The sandbox exposes `.git` read-only, so do not attempt staging or commits here.
- `codex exec` cannot initialize its app-server client while its state directory is read-only; the pinned Sol review was
  attempted with normal, read-only, and ephemeral modes and must be retried in a writable environment.
- Never deliberately background a `codex exec` run.
