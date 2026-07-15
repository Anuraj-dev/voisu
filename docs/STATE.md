# Voisu — State
> Cloud-first Linux desktop dictation app (Fedora KDE Plasma / Wayland) · Last checkpoint: 2026-07-15

## 🚧 In progress / next
- Ticket 01 is implemented and acceptance-tested, but the runner mounted `.git` read-only. Next: restore
  writable Git metadata, recreate the verified RED `test:` commit before the GREEN implementation commit,
  then run `cargo fmt` and `cargo clippy` with a complete Rust toolchain.

## Status
- Initial Cargo workspace exists with independently executed `voisu` and `voisu-daemon` binaries.
- Versioned Unix IPC proves unavailable/idle status, concurrent start rejection, stop, toggle, and protocol
  mismatch behavior through six public CLI/IPC acceptance tests.
- Audio capture, provider, validation, clock, and Delivery behavior use controlled trait-backed adapters.
- `cargo build --workspace` and `cargo test --workspace` pass; all six acceptance tests are GREEN.
- Changes remain untracked because `.git` is read-only in this runner.

## Architecture map
- Domain lifecycle, IPC contract, runtime path, boundary traits -> `crates/voisu-core/src/lib.rs`
- Tokio Unix-socket daemon and controlled adapters -> `crates/voisu-app/src/bin/voisu-daemon.rs`
- Thin public CLI -> `crates/voisu-app/src/bin/voisu.rs`
- Public-surface acceptance suite -> `crates/voisu-app/tests/daemon_cli_lifecycle.rs`

## Stack & run
- Stack: Rust 2024 + Tokio + serde · Run: `cargo run -p voisu-app --bin voisu-daemon` · Test: `cargo test --workspace`

## Key decisions (see docs/decisions.md and docs/adr/)
- `voisu-core` owns domain/protocol seams; `voisu-app` packages both process binaries for real-binary tests.
- Protocol v1 is explicit in every payload and in `$XDG_RUNTIME_DIR/voisu/v1/daemon.sock`.
- External lifecycle boundaries are synchronous traits with controlled Ticket 01 adapters; no provider/audio implementation yet.
- Existing ADRs 0001–0006 continue to govern later production implementation.

## Gotchas
- Use CONTEXT.md's ubiquitous language exactly; it lists banned synonyms.
- The runner lacks `rustfmt` and `clippy`, so those required checks remain unexecuted despite clean build/tests.
- Use an injected writable `CARGO_HOME` in the current sandbox; the default Cargo registry is read-only.
- Do not add `voisu service ...`, systemd integration, real cloud/audio adapters, or the Overlay in Ticket 01.
