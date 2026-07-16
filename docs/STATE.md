# Voisu — State
> Cloud-first Linux desktop dictation app (Fedora KDE Plasma / Wayland) · Last checkpoint: 2026-07-16

## 🚧 In progress / next
- Ticket 07 (issue #7, "Toggle Recording through the Global Shortcuts portal") is APPROVED (Sol round 3)
  and closed. Next up: Ticket 08 (08-libei-delivery, input synthesis via libei/portal RemoteDesktop) —
  likely architectural; evaluate impl at Sol medium per pinned routing, first review Sol high.
- Follow-up issue #14 remains open: make `DeepgramStream::abort` / `GroqStream::abort` cancellation-safe
  (drain→peek-then-pop) plus an abort-deadline regression test. Not blocking; pick up opportunistically.

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
- A bounded Transcript decision pipeline selects near-identical Groq text deterministically, invokes a configured Groq
  reconciliation model only for material disagreement, blocks prompt/meta/suffix/mixed-script/expansion artifacts
  (full-Unicode-range Latin/Greek/Cyrillic confusable classification), permits one bounded repair, and otherwise falls
  back to a clean Source Transcript or reports a Quality Failure. IPC evidence records selection, validation,
  fallback, reconciliation, recovery, and exactly-once Delivery outcomes, plus first chunk, capture finalization,
  per-provider completion, accepted providers, release-to-text timing, and Delivery count.
- Reconciliation cleanup follows the "pin the future, cancel, await same future under bounded grace" discipline:
  the deadline cancels the pinned reconciliation future and awaits it (bounded 1s grace) so the in-flight curl is
  killed/reaped before Idle, rather than dropping the handle; Secret Service lookup runs inside the owned
  `spawn_blocking` task; `CancelRegistry` lives in voisu-core and threads through `ReconciliationModel::request`.
- Capture/provider failure and capture EOF return the daemon to idle; Deepgram and Groq cancellation use the
  per-Recording `CancelRegistry`, owning-child kill/reap, and awaited request-task cleanup before reuse.
- Linux capture children request `PR_SET_PDEATHSIG(SIGKILL)`. Acceptance daemons run in isolated process groups whose
  Drop guard kills the whole tree, and all generated shell stubs have signal/exit traps plus bounded wait loops.
- CI is live: `.github/workflows/ci.yml` runs the workspace suite plus a 3x-parallel voisu-app flake gate on every
  push/PR; green on all commits pushed so far, including all Ticket 06 commits.
- A `DiagnosticStore` correlates each Recording under one correlation ID, retains redacted local diagnostics
  (env allowlist, URL sanitizing, secret scrub) with expiry-in-filename orphan purge, and exposes history/export/
  replay through both CLI and IPC; replay takes a fixture NAME (not a path) inside the private diagnostics/fixtures
  dir, validated to its basename with `O_NOFOLLOW` descriptor checks, and runs supervised under a `Replaying` state.
  Store IO is `create_new` 0600 with basename validation and a store mutex guarding concurrent access.
- Test inventory: 141 (3 app unit + 98 acceptance incl. 1 ignored live smoke + 20 diagnostics + 6
  provider-coordination + 14 Transcript-decision). CI green.
- Ticket 07 delivered portal-based global shortcut toggle: portal boundary traits, listener, controlled
  portal test double, and the `voisu` shortcut integration (commit d593f8a, Opus 4.8 high) — production
  zbus client was initially deferred fail-closed, which Sol ruled BLOCKING; commit b4f39ba added a
  persistent zbus client plus a real controlled GlobalShortcuts D-Bus service running on private per-test
  dbus-daemon buses (channel-file fake removed), with `VOISU_DISABLE_SHORTCUTS=1` so non-shortcut tests
  never touch the host desktop. Commit 900f456 added NameOwnerChanged rebind-loop handling (ShortcutEvent
  enum, broad Request.Response subscription, authoritative session_handle adoption, session closed on bind
  failure; mock now serves Session.Close).
- New dependency: zbus 5 (tokio feature) in voisu-app — see docs/decisions.md.

## Architecture map
- Domain, audio contract, provider coordination/timings, Transcript decision pipeline/guardrails, typed errors,
  `CancelRegistry`, IPC -> `crates/voisu-core/src/lib.rs`
- Lifecycle actor, secure socket ownership, Recording pump, Provider Deadline evidence, controlled test adapters ->
  `crates/voisu-app/src/bin/voisu-daemon.rs`
- Thin public CLI and command-specific bounded response waits -> `crates/voisu-app/src/bin/voisu.rs`
- Hardened PipeWire, Deepgram/Groq HTTP, bounded Groq reconciliation adapter, clipboard, readiness, Secret Service,
  and process adapters -> `crates/voisu-app/src/system.rs`
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
- Near-identical text uses token edit similarity and deterministic Groq selection; material differences use a bounded
  cloud Merge Result. Every candidate is guarded, with at most one bounded repair and clean-source fallback.
- Recovery remains a first-class actor state; cancellation is an `AtomicBool` observed by the wait loop owning `Child`,
  never a raw-PID signal — the same discipline now also governs the reconciliation deadline (pin future, cancel,
  bounded-grace await, never detach).
- Routing update (2026-07-16): Opus 4.8 subagents (medium/high effort) are the workhorse for regular
  implementation/fix work; Sol is reserved for architectural tickets and ALL code reviews (first review high,
  re-reviews medium).
- Replay protocol changed pre-release: IPC/CLI takes a fixture NAME inside the private diagnostics/fixtures dir,
  never a path; `DebugAudioRecord.path` was renamed to `file_name` to match.

## Gotchas
- Use CONTEXT.md's ubiquitous language exactly; it lists banned synonyms.
- `rustfmt` and `clippy` are unavailable (`sudo dnf install rustfmt clippy` needed); both remain skipped.
- This managed sandbox denies Unix-domain socket binds with `EPERM`; daemon acceptance tests compile but must run in
  the orchestrator/host gate.
- Sol's first review on Ticket 05 caught the same "detach on cancellation" bug class a third time (this time the
  reconciliation timeout dropping the `spawn_blocking` handle) — always pin-cancel-await, never let a timeout race
  drop an owned handle.
- Sol's first review on Ticket 06 found 8 real security/privacy findings in one round (path traversal via
  fixture path instead of name, env/URL/secret leakage, TOCTOU on store files, orphan expiry parsing) — diagnostics
  code touching filesystem paths or exported evidence needs security-first review, not just correctness review.
- A stale git stash ("partial edits from killed codex leak-fix run") and an older one ("partial review-fix from
  killed codex run") both remain on the stack — superseded by the merged fixes; safe to drop.
- Daemon acceptance tests now spin up private per-test `dbus-daemon` buses for the controlled
  GlobalShortcuts portal service — keep that pattern for any future portal/D-Bus work rather than reaching
  for the host session bus.
