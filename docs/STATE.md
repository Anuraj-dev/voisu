# Voisu — State
> Cloud-first Linux desktop dictation app (Fedora KDE Plasma / Wayland) · Last checkpoint: 2026-07-15

## 🚧 In progress / next
- Ticket 01 is COMPLETE and externally APPROVED after 3 review rounds. Next: dispatch Ticket 02
  (`.scratch/voisu-implementation/issues/02-verify-fedora-readiness.md` — Fedora readiness checks + Secret
  Service credentials).

## Status
- The independent `voisu` and `voisu-daemon` binaries communicate over bounded, versioned Unix IPC.
- The daemon actor exposes idle, Recording, and processing without blocking status during boundary work.
- Controlled dual-provider streams start with each Recording; their coordinator owns live chunk fan-out,
  Provider Deadline behavior, deterministic Source Transcript ordering, and one-shot completion, wired live
  into the daemon (partial-start abort, biased deadline select).
- Capture-finalization failure aborts capture, returns idle, redacts CLI errors, and permits another Recording.
- Runtime ownership is crash-safe: SIGTERM cleanup, single-instance locking, stale-socket probing, inode-owned
  cleanup, private XDG directories, and a mode-0600 socket.
- Public IPC returns ordered lifecycle evidence, exactly-once Delivery counts, and envelope-first decoding on
  both peers; the CLI reads whole frames on a bounded deadline and the stop path races a stalled provider send.
- `cargo build --workspace` and `cargo test --workspace` pass: 25 tests green (21 daemon/CLI acceptance tests
  in `crates/voisu-app/tests/daemon_cli_lifecycle.rs` + 4 provider-coordination tests in
  `crates/voisu-core/tests/provider_coordination.rs`).
- All work is committed on `main` (2331854, d288ef8, 48f6353, 63c0a97); `.git` is normal and writable.

## Architecture map
- Domain, provider coordination, typed errors, IPC contract, runtime validation -> `crates/voisu-core/src/lib.rs`
- Lifecycle actor, secure socket ownership, bounded IPC, controlled adapters -> `crates/voisu-app/src/bin/voisu-daemon.rs`
- Thin public CLI -> `crates/voisu-app/src/bin/voisu.rs`
- Public daemon/CLI acceptance suite -> `crates/voisu-app/tests/daemon_cli_lifecycle.rs`
- Provider coordination/error contract tests -> `crates/voisu-core/tests/provider_coordination.rs`
- Ordered implementation tickets -> `.scratch/voisu-implementation/issues/`

## Stack & run
- Stack: Rust 2024 + Tokio + serde · Run: `cargo run -p voisu-app --bin voisu-daemon` · Test: `cargo test --workspace`

## Key decisions (see docs/decisions.md and docs/adr/)
- `voisu-core` owns the domain/protocol seams; `voisu-app` packages both real process binaries.
- IPC v1 uses an envelope-first decoder and `$XDG_RUNTIME_DIR/voisu/v1/daemon.sock`.
- One actor serializes lifecycle transitions while spawned async work keeps processing observable.
- One Recording-scoped coordinator owns the Deepgram and Groq controlled streams and Provider Deadline.
- Boundary errors separate redacted public text from local diagnostic detail.

## Gotchas
- Use CONTEXT.md's ubiquitous language exactly; it lists banned synonyms.
- `rustfmt` and `clippy` are NOT installed on this machine (`sudo dnf install rustfmt clippy` needed) —
  skipped for all of Ticket 01; run them before relying on lint/format cleanliness.
- Never deliberately background a `codex exec` run — a deliberately-backgrounded one was killed mid-task.
  Foreground with a generous timeout; harness auto-backgrounding after a 600s timeout is fine and works.
- Orchestration/model-routing policy lives in `AGENTS.md`.
- Do not add `voisu service ...`, real cloud/audio adapters, or the Overlay until their assigned tickets.
