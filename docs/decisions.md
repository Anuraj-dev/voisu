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

## 2026-07-15 — Serialize lifecycle transitions in an actor, not a shared mutex
**Why:** The actor makes start/stop decisions atomic while long-running capture finalization, provider
coordination, validation, and Delivery run asynchronously, leaving processing observable through status.

## 2026-07-15 — Give each Recording one dual-provider coordinator
**Why:** Starting attributed Deepgram and Groq streams with the Recording and consuming the coordinator at
completion provides a seam for live chunks, deterministic ordering, a Provider Deadline, and exactly-once
completion without adding real provider behavior to Ticket 01.

## 2026-07-15 — Treat the runtime socket as a user-owned capability
**Why:** A private validated XDG root, single-instance lock, stale-socket probe, mode-0600 socket, and
device/inode-checked cleanup prevent one daemon instance from deleting or replacing another instance's path.

## 2026-07-15 — Defer rustfmt/clippy rather than block Ticket 01 on them
**Why:** Neither tool is installed on this machine (`sudo dnf install rustfmt clippy` required); blocking
approval on local tooling availability would have stalled a ticket that was otherwise fully green
(build + 25 tests) and already externally reviewed 3 times. Recorded as a gotcha to fix before relying on
lint/format cleanliness, not silently dropped.

## 2026-07-15 — Never deliberately background a `codex exec` run
**Why:** A deliberately-backgrounded codex exec was killed mid-task, losing work; the harness's own
auto-backgrounding after a 600s foreground timeout is safe and was kept as the only backgrounding path.

## 2026-07-15 — Keep cloud credentials stdin-only and Secret-Service-backed
**Why:** Command-line credential arguments would leak through shell history or process listings. `secret-tool`
receives the value on standard input; if Secret Service is denied or unavailable, the only supported fallback is
the explicit non-persistent `VOISU_GROQ_API_KEY` or `VOISU_DEEPGRAM_API_KEY` environment variable.
