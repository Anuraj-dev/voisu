# Voisu — State
> Cloud-first Linux desktop dictation app (Fedora KDE Plasma / Wayland) · Last checkpoint: 2026-07-15

## 🚧 In progress / next
- Ticket 01 review findings are fixed and verified. Next: obtain writable `.git` metadata to create the
  requested conventional RED/GREEN commits, then move to the next approved implementation ticket.

## Status
- The independent `voisu` and `voisu-daemon` binaries communicate over bounded, versioned Unix IPC.
- The daemon actor exposes idle, Recording, and processing without blocking status during boundary work.
- Controlled dual-provider streams start with each Recording; their coordinator owns live chunk fan-out,
  Provider Deadline behavior, deterministic Source Transcript ordering, and one-shot completion.
- Capture-finalization failure aborts capture, returns idle, redacts CLI errors, and permits another Recording.
- Runtime ownership is crash-safe: SIGTERM cleanup, single-instance locking, stale-socket probing, inode-owned
  cleanup, private XDG directories, and a mode-0600 socket.
- Public IPC returns ordered lifecycle evidence and exactly-once Delivery counts.
- `cargo build --workspace` and `cargo test --workspace` pass: 13 daemon/CLI acceptance tests and 3 core tests.

## Architecture map
- Domain, provider coordination, typed errors, IPC contract, runtime validation -> `crates/voisu-core/src/lib.rs`
- Lifecycle actor, secure socket ownership, bounded IPC, controlled adapters -> `crates/voisu-app/src/bin/voisu-daemon.rs`
- Thin public CLI -> `crates/voisu-app/src/bin/voisu.rs`
- Public daemon/CLI acceptance suite -> `crates/voisu-app/tests/daemon_cli_lifecycle.rs`
- Provider coordination/error contract tests -> `crates/voisu-core/tests/provider_coordination.rs`

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
- `.git` is read-only in this runner, so neither requested conventional commit could be created.
- `rustfmt` and `clippy` are unavailable on this machine and were skipped as instructed.
- Do not add `voisu service ...`, real cloud/audio adapters, or the Overlay in Ticket 01.
