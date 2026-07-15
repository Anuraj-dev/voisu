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

## 2026-07-15 — Make desktop and provider subprocesses bounded and environment-isolated
**Why:** `secret-tool` and curl must receive only the desktop-session variables they need, never inherited provider
keys, test credentials, or curl configuration. A shared async provider client centralizes the authenticated request
policy for verification and the future Groq adapter, while a bounded process runner kills stalled child processes.

## 2026-07-15 — Standardize subprocess-boundary hardening invariants across the codebase
**Why:** Four Sol review rounds on Ticket 02 converged on a consistent set of subtle process-cleanup and
resource-exhaustion defects (zombie children, descendant-pipe wedges, unbounded response buffering). Rather
than re-litigate these per ticket, they are now standing invariants for every child-process or network
boundary: `env_clear` + a minimal explicit allowlist on every spawn; `-q`-first curl; whole-operation
`Instant` deadlines on spawn, stdin-write, join, and reap; bounded joins with kill/reap on every cleanup
path (success, timeout, and error); a 16KiB cap on daemon-response bytes enforced before append; a 4KiB cap
on retained stderr with the full stream still drained; and typed, redacted errors at every boundary. Ticket
03's PipeWire/Groq/clipboard work must reuse `crates/voisu-app/src/system.rs` rather than re-implement
subprocess handling.

## 2026-07-15 — Track coder/reviewer model choice as a standing experiment
**Why:** `docs/model-benchmark.md` logs one row per codex/Opus dispatch (Sol/Terra/Luna vs Opus, task type,
review findings, fix rounds) to produce a routing recommendation after Ticket 13 instead of guessing from
memory which model performs best on which task shape.

## 2026-07-15 — Normalize PipeWire capture before provider boundaries
**Why:** A documented 16 kHz mono s16 PCM contract keeps Groq chunking deterministic regardless of the physical
microphone format. Stopping `pw-record` with SIGINT and draining its bounded stream before finalization preserves
the last spoken frames; forced kill/reap remains the bounded abort path.

## 2026-07-15 — Submit bounded Groq chunks during the Recording
**Why:** Thirty-second WAV chunks with 500 ms overlap start cloud work before stop without exposing credentials in
argv or inherited environments. The final chunk includes frames collected during graceful capture finalization;
word-overlap reconciliation produces one validated Groq Source Transcript and therefore one clipboard Delivery.

## 2026-07-15 — Reject (do not defer) Start during post-failure recovery
**Why:** After a failed start, the daemon enters a Recovering state until the bounded capture/provider aborts
acknowledge completion. Start/Toggle received meanwhile get an immediate, distinct retryable rejection
("Recording recovery in progress; retry shortly") instead of being queued for replay. A deferral queue was
tried and rejected: a Stop can overtake a deferred Start (reordering Start→Stop into a live Recording), two
deferred Toggles misbehave, and a deferred Start can begin a Recording after its client already timed out —
a ghost Recording nobody observes. Rejection preserves command ordering by construction and never starts a
Recording without a live client; callers retry, which the CLI acceptance helper encodes.

## 2026-07-15 — Provider aborts must kill registered subprocesses, not just tasks
**Why:** Aborting a tokio task that awaits `spawn_blocking` curl work detaches the blocking subprocess, which
would keep an aborted Recording's provider request alive for up to its 14 s deadline and overlap the next
Recording. Each Groq stream owns a per-Recording cancel registry of in-flight curl child pids; abort marks it
cancelled and SIGKILLs the registered children (the owning bounded-wait loop reaps them), and per-Recording
registry ownership guarantees stale results die with their aborted stream.
