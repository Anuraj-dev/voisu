# Voisu — State
> Cloud-first Linux desktop dictation app (Fedora KDE Plasma / Wayland) · Last checkpoint: 2026-07-15

## 🚧 In progress / next
- Ticket 02 security review findings are fixed and verified; Git metadata is read-only (`.git/index.lock` cannot be
  created), so commit `fix(auth): harden Ticket 02 subprocess boundaries (#2)` when writable, then dispatch Ticket 03.

## Status
- The independent `voisu` and `voisu-daemon` binaries communicate over bounded, versioned Unix IPC.
- The daemon actor exposes idle, Recording, and processing without blocking status during boundary work.
- Public `voisu doctor` reports PipeWire, the default microphone, portal service, clipboard roundtrip, Secret Service
  lookup, and a versioned daemon status handshake as actionable PASS/WARN/FAIL outcomes.
- `voisu auth set <groq|deepgram>` reads a credential only from standard input and replaces its desktop Secret Service
  entry. `voisu auth verify <provider>` loads Secret Service (or the explicit development/headless environment fallback)
  and makes a response-discarding authentication check.
- Boundary errors redact credentials from CLI errors. The normal adapters send credentials to `secret-tool` and `curl`
  through standard input, use bounded child waits, and clear inherited environments except desktop-session allowlists.
- `cargo build --workspace` and `cargo test --workspace` pass: 36 tests green (32 public daemon/CLI acceptance tests
  and 4 provider-coordination tests). `git diff --check` passes.

## Architecture map
- Domain, provider coordination, typed errors, readiness/secret/auth traits, IPC contract, runtime validation ->
  `crates/voisu-core/src/lib.rs`
- Lifecycle actor, secure socket ownership, bounded IPC, controlled Recording adapters ->
  `crates/voisu-app/src/bin/voisu-daemon.rs`
- Thin public CLI -> `crates/voisu-app/src/bin/voisu.rs`
- Reusable Fedora readiness, Secret Service, and async provider HTTP adapters -> `crates/voisu-app/src/system.rs`
- Public daemon/CLI acceptance suite -> `crates/voisu-app/tests/daemon_cli_lifecycle.rs`
- Provider coordination/error contract tests -> `crates/voisu-core/tests/provider_coordination.rs`
- Ordered implementation tickets -> `.scratch/voisu-implementation/issues/`

## Stack & run
- Stack: Rust 2024 + Tokio + serde · Run: `cargo run -p voisu-app --bin voisu-daemon` · Test: `cargo test --workspace`

## Key decisions (see docs/decisions.md and docs/adr/)
- `voisu-core` owns the domain/protocol seams; `voisu-app` packages both real process binaries.
- IPC v1 uses an envelope-first decoder and `$XDG_RUNTIME_DIR/voisu/v1/daemon.sock`.
- One actor serializes lifecycle transitions while spawned async work keeps processing observable.
- Credentials are stdin-only, Secret-Service-backed values; environment variables are an explicit non-persistent fallback.
- Boundary subprocesses use explicit environment allowlists and deadlines; the async provider client owns curl policy.
- Boundary errors separate redacted public text from local diagnostic detail.

## Gotchas
- Use CONTEXT.md's ubiquitous language exactly; it lists banned synonyms.
- `rustfmt` and `clippy` are NOT installed on this machine (`sudo dnf install rustfmt clippy` needed); they remain skipped.
- The current sandbox makes `.git` metadata read-only: staging and commits cannot create `.git/index.lock`.
- Never deliberately background a `codex exec` run.
- Do not add `voisu service ...`, real cloud/audio adapters, or the Overlay until their assigned tickets.
