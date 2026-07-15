# Decisions — Voisu
> Append-only log of load-bearing choices and WHY. Newest at the bottom.
> Format: `## YYYY-MM-DD — <decision>` then a short **Why:** line.
> Hard-to-reverse architectural decisions live in `docs/adr/` — this log is for everything lighter.

## 2026-07-15 — Adopt ADRs 0001–0006 as governing architecture (inferred at adoption)
**Why:** See `docs/adr/` — cloud-only dual-provider transcription, independent Rust codebase,
daemon/Overlay separation, portals-only input access, bounded quality wait, local-only diagnostics.

## 2026-07-15 — Keep both process binaries in one application crate
**Why:** `voisu-core` remains a reusable domain/protocol crate while `voisu-app` packages the independent
CLI and daemon executables, allowing Cargo acceptance tests to discover and drive both real binaries
without test-only binary lookup hooks.

## 2026-07-15 — Version IPC in both the socket path and every payload
**Why:** `$XDG_RUNTIME_DIR/voisu/v1/daemon.sock` prevents accidental cross-major socket discovery, while
request and response version fields let both peers reject incompatible payloads explicitly.
