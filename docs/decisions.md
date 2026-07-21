# Decisions — Voisu
> Append-only log of load-bearing choices and WHY. Newest at the bottom.
> Format: `## YYYY-MM-DD — <decision>` then a short **Why:** line.
> Hard-to-reverse architectural decisions live in `docs/adr/` — this log is for everything lighter.

## 2026-07-16 — Keep Overlay feedback on a separate observer status command
**Why:** The GTK Overlay uses read-only `OverlayStatus` so terminal Delivered or Quality Failure feedback does not change established CLI `Status` output; the Overlay remains disposable and cannot mutate daemon state.

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

## 2026-07-15 — Provider aborts must kill their subprocesses, not just tasks — via a cancel flag, never raw pids
**Why:** Aborting a tokio task that awaits `spawn_blocking` curl work detaches the blocking subprocess, which
would keep an aborted Recording's provider request alive for up to its 14 s deadline and overlap the next
Recording. A first design that stored in-flight child pids and SIGKILLed them from abort was rejected: once a
child is reaped elsewhere its pid can be recycled by the kernel, so raw-pid signaling can kill an unrelated
process. Instead, each Groq stream owns a per-Recording cancel FLAG; abort only sets it, and the bounded-wait
loop that owns each `Child` observes the flag every ~10 ms poll tick and kills through its own `Child` handle —
pid-reuse-safe because that loop is the only reaper. Already-cancelled operations fail fast without spawning,
and per-Recording flag ownership guarantees stale results die with their aborted stream.

## 2026-07-15 — Recovery is a first-class actor state with retryable rejection, not a deferral queue
**Why:** `ActorState::Recovering(u64)` rejects Start/Toggle during recovery with a retryable error instead of
queuing them, because queuing risked ordering violations across recovery boundaries. A `Recovered(id)` ack
gates the return to `Idle`, and `abort_recording_work` runs capture abort and provider-coordinator abort
concurrently via `tokio::join!` inside a 2s `RECOVERY_ABORT_DEADLINE`, itself inside the 22s
`PROCESSING_RESPONSE_DEADLINE`.

## 2026-07-15 — Cancel subprocesses via an AtomicBool flag, not raw-PID signals
**Why:** Sol's Ticket 03 review (HIGH finding) identified a PID-reuse race in the original raw-PID `SIGKILL`
cancellation. `CancelRegistry` now sets an `AtomicBool`; only the bounded-wait loop that already owns the
`Child` handle acts on it, killing via that handle on a ~10ms poll tick, and already-cancelled operations
fail fast without spawning.

## 2026-07-15 — Review effort policy: first review high, re-reviews medium
**Why:** Ticket 03 needed 5 Sol review rounds; running every round at high effort wastes Codex quota once the
first pass has surfaced the architecture-level findings. First review of a ticket runs Sol high; subsequent
re-reviews until merge run Sol medium. Recorded in `AGENTS.md`/`CLAUDE.md`.

## 2026-07-15 — Stream Deepgram through the existing hardened HTTP process boundary
**Why:** The approved specification requires Deepgram to receive audio during the Recording but does not mandate a
WebSocket library. One-second linear16 HTTP chunks begin cloud work live, preserve the existing stdin-only credential,
`env_clear`, response-cap, cancellation, and owning-child kill/reap guarantees, and avoid adding a second networking
stack. Voisu's 14-second whole-operation `Instant` budget intentionally expires before curl's 15-second internal limit,
so Voisu consistently owns Provider Deadline classification and cleanup instead of racing curl's exit status.

## 2026-07-15 — Queue Deepgram chunks behind a three-request in-flight cap
**Why:** A slow endpoint must not turn a five-minute Recording into hundreds of simultaneous curl processes and
pipe-drain threads. Deepgram queues request tasks behind a three-permit semaphore, so audio ingestion stays live while at
most three tasks can own curl processes. Completion awaits the queued handles in creation order, preserving every
non-overlapping audio chunk and transcript order. Coalescing was rejected because it would change request boundaries.

## 2026-07-16 — Keep Transcript reconciliation and recovery behind one bounded decision boundary
**Why:** Near-identical Source Transcripts should avoid cloud latency, while material disagreement benefits from a
configured Groq Merge Result. The validator boundary now owns deterministic selection, a 3s reconciliation deadline,
candidate guardrails, at most one repair, and clean-source fallback before returning one Transcript to Delivery. The
curl child has its own shorter 2s owning deadline, so dropping the outer future cannot leave indefinite work behind;
delivering a first candidate and correcting it later was rejected because it violates exactly-once Delivery.

## 2026-07-16 — Bind the Trigger Key on a persistent native zbus connection, not per-call subprocesses
**Why:** The `org.freedesktop.portal.GlobalShortcuts` portal resolves request/session handles against the caller's
D-Bus identity and delivers `Activated` signals only to the connection that created and bound the session, so the
repo's established per-call `busctl`/`gdbus` subprocess edge can create a session but can never receive its own
activations — a long-lived in-process client is structurally required (confirmed in Sol's Ticket 07 review). The
daemon therefore takes `zbus` (default features off, `tokio` integration) and holds one persistent session-bus
connection owning the Global Shortcuts session; the listener subscribes to `Activated`/`Closed` before binding,
fails closed on an absent or denying portal, and closes the session on retirement. Acceptance tests keep the edge
substituted by pointing `DBUS_SESSION_BUS_ADDRESS` at a private `dbus-daemon` (service activation disabled) running
a controlled portal service, so the production client is exercised over a real bus without touching the host desktop.

## 2026-07-16 — Preserve the Transcript before background-prepared libei Delivery
**Why:** Direct Delivery must never strand speech when desktop permission, the RemoteDesktop portal, EIS TEXT
capability, the connection, or the focused application fails. The daemon therefore writes the final Transcript to
the clipboard first and only reports direct success after a bounded libei frame plus pong. RemoteDesktop setup runs
in the background with persistent keyboard permission (`persist_mode=2`) on one zbus connection, so an approval
dialog cannot extend stop-to-Delivery latency; pending or failed setup is an explicit clipboard fallback. libei is
loaded by SONAME at runtime and TEXT is required, preserving Unicode independently of the active layout without a
build-time `libei-devel` dependency or an unsafe raw-input/`uinput` alternative.

## 2026-07-16 — Report compositor submission honestly and support libei 1.5
**Why:** Sol review confirmed that a libei pong acknowledges compositor processing, not focused-application
acceptance, and Fedora 43 ships libei 1.5 without TEXT. Delivery evidence therefore reports
`compositor_submitted`, never application acceptance. libei 1.6 TEXT remains preferred; 1.5 resolves Ctrl+V from the
EIS-provided active XKB keymap and submits the already-preserved clipboard. RemoteDesktop restore tokens rotate in a
private 0600 state file, while denial or revocation is terminal for the daemon lifetime to avoid repeated prompts.

## 2026-07-16 — Install one graphical-session-owned user service from an atomic daemon copy
**Why:** A unit that points into a checkout becomes stale across rebuilds, while embedding display, Wayland, D-Bus,
or authorization values becomes stale across logins. `voisu service install` therefore atomically copies the trusted
sibling `voisu-daemon` into the XDG user data directory and writes one user unit containing only that stable path.
The unit is enabled by `graphical-session.target`, ordered after D-Bus, PipeWire, and the desktop portal, and stopped
with the graphical session. Management reports both systemd state and versioned IPC state. A manual daemon wins
without being killed: the CLI avoids starting a duplicate, and the `--systemd` race guard exits cleanly so
`Restart=on-failure` cannot loop. Upgrade swaps the executable inode before restarting an already-managed service;
uninstall disables first, waits for ownership and IPC to clear, then removes the unit, executable, and stale socket.

## 2026-07-16 — Codex dispatch prompts are self-contained (no Claude delegation)
**Why:** GPT/codex agents prompt `claude -p` poorly, wasting tokens on both sides. Rejected alternative: keeping the
mandatory delegation-to-Claude block in every codex dispatch. Codex now gets all needed context inline; Claude-side
subagents remain the orchestrator's tool only.

## 2026-07-16 — Bound persistent service failure without retrying Recording work
**Why:** `Restart=on-failure` is useful for abrupt daemon interruption but an unrecoverable startup defect must not
spin forever. The user unit permits three starts per 30 seconds, while microphone, provider, portal, CLI, and
Delivery failures stay inside one Recording and recover to a fresh next Recording. Retrying or replaying a failed
Recording was rejected because it risks duplicate Delivery and ghost cloud work.

## 2026-07-16 — Guard every external child against abrupt owner death
**Why:** Bounded cancellation cannot run after an uncatchable process interruption, so each PipeWire, provider,
clipboard, secret-store, and systemctl child must also have a kernel-enforced owner-death contract. One shared Linux
spawn hook sets `PR_SET_PDEATHSIG=SIGKILL` and refuses exec when the expected parent is already gone, closing the
fork-to-prctl race. Per-command hooks were rejected because they had already left provider and service children
uncovered and allowed the PipeWire hook to omit the race check.

## 2026-07-16 — Ship one GTK-free Fedora RPM with an optional Overlay subpackage
**Why:** The release candidate is built by `rpmbuild` from a Cargo.lock-pinned source archive created from the exact
tested git commit. The base package owns `/usr/bin/voisu`, `/usr/bin/voisu-daemon`, and one graphical-session user unit;
`voisu-overlay` owns the optional GTK4 binary and dependencies. A packaged unit is preferred over Ticket 09's XDG
user-data copy, which is migrated and removed on upgrade so a stale executable cannot silently own the daemon. RPM
scriptlets disable the unit on removal while leaving credentials, supported state, and diagnostics for the user.
## 2026-07-16 — Version terminal Overlay feedback independently of daemon lifecycle status
**Why:** The Overlay needs display-once terminal feedback without making CLI Status sticky or coupling presentation to Recording/Delivery ownership. Typed event IDs let an observer deduplicate and expire feedback while the daemon remains authoritative and reusable.

## 2026-07-16 — Keep Overlay fallback and supervision outside the daemon (superseded 2026-07-16 — see below)
**Why:** GTK Layer Shell support belongs to the running compositor, not Cargo target selection. The Overlay therefore
selects Layer Shell only after the GTK runtime advertises it; X11 and unavailable Layer Shell use an unfocusable
regular GTK surface, while missing display, GTK, or a failed surface select desktop-notification feedback with a
specific degradation reason. `voisu status` deliberately remains daemon-only: the separate observer emits its chosen
backend and reason to structured stderr/journal logs and `voisu-overlay --report-backend`. `voisu-overlay --supervise`
limits its own failures to three in 30 seconds and has no daemon IPC command, signal, or lifecycle path; restarting the
daemon to recover presentation was rejected because it could interrupt a Recording or duplicate Delivery.

## 2026-07-16 — Report only Overlay degradations the running process can observe (surface-map claim superseded 2026-07-16 — see below)
**Why:** `voisu-overlay` dynamically links GTK, so a missing GTK runtime fails in the ELF loader before `main`; it cannot honestly select or log a `missing-gtk-dependency` backend. The launching systemd unit and journal are the explicit failure record for that case. With no display, the Overlay instead remains a read-only status observer and writes transition logs as its real last-resort feedback surface. A Wayland session without `WAYLAND_DISPLAY` but with `DISPLAY` uses a named XWayland regular-surface fallback. Surface success requires a bounded GTK map signal after `present`, not merely a locally realized surface; a windowless desktop-notification backend holds its application for its polling lifetime.

## 2026-07-16 — Overlay surface creation is local realization, not a compositor map probe (round-2)
**Why:** Round-1 declared surface success only after a bounded GTK `map` signal following `present()`. That probe was unsound: `GtkWidget::map` reflects GTK's local widget lifecycle, not compositor acceptance, so the flag turned true even when the Wayland surface later failed, and a locally delayed map beyond the 500 ms grace produced a false, permanent desktop-notification fallback on a perfectly healthy compositor. The Overlay now treats successful GTK realization (`window.surface().is_some()` on the first real show) as surface creation, and falls back to a desktop notification only when GTK realizes without a surface — the sole in-process-detectable surface failure. A compositor that *rejects* the surface (e.g. a Layer Shell protocol error) instead raises a Wayland protocol error that terminates the process; the bounded `voisu-overlay --supervise` policy, not a false in-process timer, converts that into explicit degraded behavior. A false fallback on a healthy compositor is therefore impossible. This chooses acceptable direction (a): drop the pretense of compositor confirmation and keep an honest, testable story. The window also stays hidden at Idle — no startup `present()` and no styled empty-capsule flash — and becomes visible only when a visible phase arrives, while status polling starts immediately so an early Recording is never missed. Reintroducing a map/timeout probe was rejected as dishonest; a startup `present()` was rejected as a DESIGN.md 'hidden at Idle' violation.
## 2026-07-16 — Make the Fedora RPM build offline with an exact-commit vendor archive
**Why:** A Cargo.lock alone does not make a clean mock build reproducible when crates are not present in the build
root. `packaging/build-rpm.sh` therefore creates `Source1` with `cargo vendor --locked` from the same clean commit,
and the spec writes a Cargo source replacement before every offline build/check. Fetching crates during `%build` or
`%check` was rejected because it would make the tested artifact depend on network state.

## 2026-07-16 — Validate packaged service ownership before migrating Ticket 09 data
**Why:** A regular packaged unit file without `/usr/bin/voisu-daemon` could make `voisu service install` report
success for a service that cannot start. Detection now validates the executable and exact `ExecStart`, then clearly
falls back to the Ticket 09 user-data path. RPM removal also requires the desktop user's uninstall command first,
because systemd user scriptlets cannot reliably clear live per-user ownership and enablement.

## 2026-07-17 — Model external tools by their real behavior, not their documented contract
**Why:** The live desktop smoke disproved two assumptions the whole test suite had encoded: `wl-copy` forks a
clipboard-serving child that inherits the parent's pipes (so capturing its output reads the healthy case as a
timeout — its output is now discarded and only the parent's exit status is trusted), and `pw-record` catches SIGINT
and exits 1 silently instead of dying by the signal (so a nonzero exit is accepted only when the child was alive at
the interrupt and stderr is empty; a capture already dead before stop still fails and never delivers). The
alternative — keeping strict status contracts and wrapping the tools — was rejected because the tools' real shapes
ARE the boundary contract; realistic test fakes now encode them.

## 2026-07-17 — Keep the Overlay on GTK4 + gtk4-layer-shell; do not migrate to Electron
**Why:** Evidence review (two parallel research agents: HyprVox's Electron overlay + web research on
KWin/Wayland). For Voisu's constraints — a click-through, layer-anchored, disposable capsule on KDE/KWin,
shipped as a lightweight RPM subpackage, driven by the Rust daemon — GTK4 is the only option that can
request a real `zwlr_layer_shell_v1` surface from KWin (default `keyboard_interactivity=none`, empty input
region for click-through), adds ~70KB on top of already-present GTK, and stays in one native Rust process
via gtk4-rs. Electron/Chromium has no layer-shell support on Wayland (`setAlwaysOnTop` is a no-op,
positioning broken), forcing a non-scriptable WM-rule hack + a 150–400MB Chromium runtime + a cross-process
JS bridge to the Rust daemon. HyprVox chose Electron, but only via forced XWayland self-positioning +
Hyprland-specific window rules and a rich React/canvas waveform — context that does not transfer to Voisu.
Rejected: an Electron migration, which would be a rewrite away from the correct architecture. Full weighted
comparison in the 2026-07-17 session log. This affirms ADR-0003's daemon/Overlay split rather than reversing
it; promote to a dedicated ADR only if the toolkit floor (min KWin/Plasma version for layer-shell) needs to
be pinned as a hard dependency.

## 2026-07-17 — Start the optional Overlay through its own graphical-session user unit
**Why:** The Overlay RPM previously shipped only the healthy binary, so no login path launched
`voisu-overlay --supervise`. The optional subpackage now owns an independent `graphical-session.target`
user unit, while `voisu service install|uninstall` manages it only when its effective fragment and
`ExecStart` still resolve to the packaged Overlay and treats every Overlay failure as non-fatal.
`After=voisu.service` provides ordering without `Wants=` or `Requires=`; daemon start,
Recording, Transcript production, and Delivery never depend on presentation. A separate CLI verb was
rejected as unnecessary setup friction, and XDG autostart was rejected because it diverges from the
existing observable systemd-user lifecycle.

## 2026-07-17 — Transcription accuracy overhaul design (PRD)
**Why:** Blind test measured 26.3% WER; evidence (recordings 11–14) showed the
real causes were Deepgram's context-free 1 s batch chunks and an unprompted
Groq call — not reconciliation, refuting the prior hypothesis. Chose Groq
full-audio-at-finalize ≤120 s (tail request already costs ~0.5 s, so no
latency penalty) + `whisper-large-v3` default (Groq free tier covers both
models at 2 h/day; accuracy decides) + shared built-in/user vocabulary
dictionary feeding Whisper `prompt` and Deepgram `keyterm`; Deepgram rebuilt
as real nova-3 websocket streaming (batch-on-finalize rejected: doubles
release latency; dropping it rejected: Raja keeps the second opinion and will
rotate credit accounts) and must stay disableable (only non-free component).
Full spec: docs/specs/2026-07-17-transcription-accuracy.md

## 2026-07-17 — Latency optimization effort: Deepgram toggle, FLAC, keep curl, fix delivery
**Why:** `voisu history` recs 20–39 showed the release-to-text tail is ~1889 ms
with Deepgram vs ~690 ms Groq-only (~400 ms floor). Deepgram gates the
wait-for-both barrier 12/12 times and its 282 ms RTT from India is structural,
not code-fixable; reconciliation also strips proper nouns. Four decisions
(grilling with Raja): **(D1)** Deepgram becomes a default-off runtime toggle
(`voisu deepgram on|off`, persisted) — evaluate Groq-only live, then finalize
delete-vs-keep, rather than a blind deletion; harmonizes with the accuracy map
which already required Deepgram be disableable. **(D2)** Keep `curl`; defer TLS
warm-up + pooled reqwest client to future ambition (the ~70 ms win isn't worth
ripping out the curl security sandbox + a security re-review now). **(D3)** FLAC
lossless upload, not Opus — zero WER risk against the ≤10% bar. **(D4)** Fix
direct-typing delivery (`fix/delivery-keymap-fd`) first; auto-paste keystroke
synthesis only as 2nd-best if direct-typing is unreliable on the host. Sequenced
AFTER the accuracy branch integrates (shared `system.rs`/`lib.rs`/daemon files).
Full plan: `docs/specs/2026-07-17-latency-optimization.md`; map + tickets:
`.scratch/voisu-latency/`.

## 2026-07-17 — Three-strike subagent escalation rule
**Why:** Ticket 06 (reconciliation divergence gate) proved that a subagent stuck
after repeated review rounds keeps patching symptoms rather than fixing the
design — Opus burned three rounds (`54e29ff`/`d63b8a4`/`d06062a`) accumulating
complexity without converging. New rule: **3 failed review rounds → discard that
agent and respawn a FRESH agent at higher effort, handing it the full findings
history** (and a simplify mandate if the failures were complexity-driven). Proven
this session: after the discarded Opus, a fresh Fable agent still failed 3 rounds
(`bd34220`/`bc01840`/`3d2e2c2`), but a second fresh Fable with an explicit
simplify mandate delivered the accepted design in one shot (`4f71124`, single
symmetric `phonetic_matching` feeding gate + selection) plus one alignment fix
(`b2b83a0`), Sol APPROVE. Findings-per-round fell 6→3→2→3→5→1→0. Rejected
alternative: continuing to patch with the same agent, which the six wasted rounds
show does not converge — a fresh context beats an entrenched one.

## 2026-07-18 — Post-audit hardening sequencing + trigger-ruled refactors
**Why:** Full audit found 2 criticals (daemon wedge via unsupervised
capture_pump panic; blocking stop_child on Tokio workers) living in the same
files latency tickets 01/04 touch. Decided: criticals land right after the
accuracy branch integrates and BEFORE latency (small diffs rebase cheap;
rejected "everything waits" as it delays live correctness fixes). systemd
hardening + CI cargo-audit/clippy are parallel-safe anytime (packaging/CI only).
Minors bundled into one hygiene ticket behind latency (rejected per-ticket
tracking overhead). Big refactors get TRIGGER RULES, not queue slots —
system.rs split before overlay/desktop-target work, provider collectionization
before a 3rd provider, nested-compositor e2e with the overlay milestone and
CI-ONLY (Raja: local e2e eats his machine) — rejected firm scheduling to avoid
churning files twice. Map: `.scratch/voisu-hardening/`.

## 2026-07-18 — Spawn external children from long-lived threads (pdeathsig rule)
**Why:** PR_SET_PDEATHSIG delivers on the death of the forking THREAD, not the
process. pw-record was spawned inside `spawn_blocking`; Tokio reaps idle
blocking-pool threads after ~10 s, so every recording longer than that was
SIGKILLed. Fix: the capture reader thread (which lives as long as the child)
spawns pw-record itself and hands the Child back over an mpsc channel. Rejected
alternatives: dropping pdeathsig (loses orphan cleanup) and raising the pool
keep-alive (fragile, global). Regression test pins /proc starttime + non-zombie
+ daemon status; a `VOISU_TEST_BLOCKING_KEEP_ALIVE_MS` seam shrinks keep-alive.

## 2026-07-18 — rustls crypto backend = ring, installed explicitly at startup
**Why:** tokio-tungstenite's `rustls-tls-webpki-roots` selects no crypto
backend; rustls 0.23 then panics at first TLS use ("no process-level
CryptoProvider"). Chose ring (pregenerated asm, no cmake) over aws-lc-rs
(cmake would break the vendored offline RPM build); `install_crypto_provider()`
is idempotent and runs first in daemon main. Ring's static link adds
`Apache-2.0 AND ISC` to the RPM License tag and its texts ship via %license.

## 2026-07-18 — Read EIS/portal keymap fds with pread at offset 0
**Why:** the keymap fd is a shared open file description whose offset the
compositor leaves at EOF after populating the memfd via write(); reading
through the cursor yielded empty input and xkbcommon parse failures, stranding
every Delivery on clipboard_fallback. pread neither depends on nor mutates the
shared offset (also seal-proof). Alternative rejected: lseek(0) before read —
it mutates shared state another reader may depend on.

## 2026-07-18 — Guaranteed-completion supervisor paths must be panic-free
**Why:** A supervisor that panics before sending Completed/Recovered/ReplayCompleted
re-creates the permanent wedge it exists to prevent. Concretely: DiagnosticStore
locks recover poison (`lock_store`; the mutex guards only file-cycle serialization,
disk writes are atomic); logging uses best-effort writes (`log_best_effort` /
`let _ = writeln!`) because `eprintln!` panics on failed stderr writes; no new
persisted enum discriminators (old binary's `load_raw` turns one unknown variant
into a silent full-history wipe on rollback). Alternative rejected: catch_unwind
wrapping — fixing each panic source is simpler and reviewable.

## 2026-07-18 — Idle is gated on reaper drain_to_completion; cleanup is adopted, never detached
**Why:** spawn_blocking made stop_child cancellation-unsafe (an outer 2 s timeout
could drop the future and detach live pw-record cleanup). Instead of enlarging
outer deadlines (fragile coupling), all cancelled or dropped capture/provider
cleanup is adopted into the actor-owned ProviderReaper (retained JoinHandle on
cancel; `adopt_capture_blocking` on pre-stop Drop, runtime-free) and every
Idle-permitting path drains to completion — the next Recording can never overlap
the previous one's cleanup. Each retained cleanup is internally bounded, so the
drain terminates; the service unit's TimeoutStopSec is the last-resort backstop.

## 2026-07-18 — cladex adopted as the Sol dispatch channel; ~50/50 Claude/Codex split (Raja)
**Why:** cladex (Claude Code harness → local CLIProxyAPI → gpt-5.6-*) gives Sol
full harness tooling, JSON usage capture, and native `--effort` passthrough
(verified in the proxy debug log), while billing the Codex Plus quota. Raja's
budget directive: split work ~50/50 — Sol keeps architecture-grade implementation
and all reviews; Claude (driver/Opus) takes scoped lifecycle fixes, point work,
reconciliation, and docs. Codex CLI remains the fallback if the proxy misbehaves.

## 2026-07-18 — Disabled Deepgram is an adapter stand-in, not coordinator surgery
**Why:** `DisabledProvider` completes the barrier instantly with the canonical
"Deepgram disabled for this Recording" diagnostic, so `ProviderCoordinator`,
supervision, and the reaper stay untouched — smallest blast radius, ON-path
byte-for-byte identical. Rejected: threading an enabled-flag through the
completion barrier (touches hardened guaranteed-completion code).

## 2026-07-18 — FLAC upload ships ungated by duration
**Why:** measured short-clip encode cost ~3 ms (2 s clip) for a 42% payload
cut, so a WAV fallback branch for short Recordings buys nothing and keeps two
payload formats. Rejected: duration gate proposed in the ticket.

## 2026-07-18 — Test assertions pin only the deterministic pre-stop capture
**Why:** post-signal bytes from a capture child are best-effort by design (stop
adopts the capture into the reaper rather than guaranteeing further reads), so
asserting them is a race — it broke CI once (PR #28). Rejected: retrying/
sleeping the assertion, which would mask genuine drain regressions.

## 2026-07-18 — CI clippy gate ships with justified crate-root allows
**Why:** `result_large_err`/`large_enum_variant`/`too_many_arguments` reflect
deliberate rich-diagnostic and supervisor shapes; boxing/shrinking them is a
cross-cutting change owned by the hardening-05 hygiene sweep. The alternative —
blocking the gate on that refactor — would leave main unprotected meanwhile.
Each allow carries a comment naming the debt.

## 2026-07-18 — systemd sandbox directives merge only after real-install validation
**Why:** unit-file linting can't prove PipeWire/portal/Secret-Service/libei
still work under `ProtectSystem=strict` + address-family restrictions, and
merging before the live latency eval could confound its measurements. PR #31
stays open until Raja's install validates every directive. Alternative rejected:
merging on `systemd-analyze verify` alone.

## 2026-07-18 — Friends-distribution decisions (research-backed, grilling session)
**Why:** 13-scout Sonnet fleet + adversarial fact-check (benchmark rows 122–134;
evidence digest `.scratch/voisu-research/2026-07-18-distribution-decisions.md`).
- **GTK4 locked, Electron rejected** — Chromium/Ozone has no layer-shell path
  (overlay impossible natively); 150–250 MB + CVE treadmill; alternative
  rejected: Electron; Tauri is the only acceptable web-tech fallback. ADR queued.
- **delivery_mode enum type|clipboard|guarded, default type; guarded built now**
  — focus-guard (auto-type only if focus unchanged) ships as differentiator;
  alternative rejected: bool flag (blocks guarded without migration).
- **STT stays two-mode** — no Deepgram-only third mode (est. 300–500 ms, not
  clearly faster than Groq's 474 ms floor; extra config surface not worth it).
- **Packaging:** cargo-deb + AUR source/-bin + COPR (vendored crates) +
  self-hosted GPG apt repo, one on-tag GH Actions workflow with container
  install-smoke gates; Launchpad PPA rejected (offline builders); Flatpak later
  (flatpak#2787), AppImage never. GNOME gets plain-window fallback now, Shell
  extension later; Ubuntu floor 24.10+ (Kubuntu 24.04 = Plasma 5.27).
- **Onboarding: pure BYOK** (free tiers cover friends), `voisu setup` wizard w/
  live key validation, keyring + loud 0600 fallback; shared relay rejected.
- **Vocab scope:** keyterm cap fix (bug: uncapped → Deepgram 400 kills stream) +
  dictionary CLI + per-session hot-reload; replacements tier deferred.
- **Process:** fixes → features → packaging; implementer failing 2 review rounds
  is discarded (Fable inline or higher-effort respawn) — tightened from 3.

## 2026-07-19 — Codex/GPT models are reviews-only; Claude implements everything
**Why:** Raja's Codex Plus quota is nearly exhausted while Claude Max headroom is ample. All
implementation → Opus 4.8 subagents (Fable 5 subagent for genuinely architectural work; the driver
never implements — it orchestrates). Sol keeps all reviews (first high, re-reviews medium). Rejected:
continuing Terra/Luna feature dispatches (would burn the remaining review budget).

## 2026-07-19 — Guarded delivery: strict stable_id guard, all-string KWin wire, bounded staleness
**Why:** (1) Comparison key is stable_id ONLY — same-app-different-window is a mismatch; the guard's
promise is "the surface you started in", and app-level matching would type into the wrong window.
(2) KWin callDBus marshals JS Numbers as INT32, which zbus rejects against u32 — silently defeating
every push; the D-Bus surface is all-strings with Rust-side parsing (Opus review catch). Pushes are
sender-authenticated against KWin's unique owner. (3) The 10-min staleness deadline stays: it bounds
the fail-open window if the push-only script dies while KWin lives; the long-dwell clipboard fallback
is the accepted, recoverable cost. Rejected: trusting owner-check alone (unbounded fail-open) and a
script-side timer (KWin sandbox has no reliable timer).

## 2026-07-19 — Dictionary edits flock-serialized; hot-reload is one snapshot per Recording
**Why:** Concurrent CLI edits raced (read-modify-write, last rename wins) — flock(2) on a sibling
lock file serializes writers while readers stay lock-free on atomic rename. Hot-reload re-reads the
dictionary once at each Recording start and threads that snapshot into BOTH providers; the supervised
replay tail keeps its captured snapshot (no fs I/O invariant preserved). Rejected: full Unicode case
folding (disproportionate dependency) and Deepgram reconnect-without-keyterms (cap already prevents
the 400).

## 2026-07-19 — GNOME fallback: re-present on visible transitions; notify from observed daemon states
**Why:** Wayland forbids keep-above for regular toplevels, so the fallback window re-present()s once
per rendered transition into a visible phase. The Recording notification derives from OBSERVED daemon
states (RecordingNotifyLatch): an unreachable blip mid-Recording must not re-notify — rendered-phase
edges alone refire across the unavailable capsule. Clipboard keeps the wl-copy shell-out (speaks
wl_data_device, works on GNOME); Flatpak-proofing deferred to phase-B packaging.

## 2026-07-20 — Trigger Key permanence = portal refusal only (PR #61)
**Why:** XDG protocol gives Session.Closed no reason field, and connection death is indistinguishable
from benign compositor resets — treating either as revocation left the shortcut permanently dead after
reboot/suspend (GH #60). Only an explicit response-1 refusal retires the listener; everything else
rebinds on bounded backoff. Rejected: a listener-side one-reattempt budget (would re-break self-healing
when a benign reset's first rebind fails transiently).

## 2026-07-20 — No `keyring` crate; secret-tool boundary for key storage (PR #62)
**Why:** both keyring backends drag a duplicate D-Bus stack (C libdbus or zbus 4 + async-io/aes) beside
our zbus 5 — COPR vendored-build bloat for zero capability gain. The existing secret-tool boundary
delivers keyring-primary + loud 0600 fallback + no startup blocking with zero new deps. Reviewer-endorsed.

## 2026-07-20 — Fallback-file prune honesty: content-keyed tri-state, classified once (PR #62)
**Why:** three review rounds of false alarms proved existence checks lie (file present ≠ target
provider's line present; lock uncreatable ≠ copy survived). Outcome (gone/survived/unverifiable) is
keyed on the target provider's line and classified solely inside FileSecretStore::remove; callers relay.
All fallback-file mutations flock-serialized (CredentialsLock mirrors DictionaryLock).

## 2026-07-20 — Packaging credential architecture (ticket 09)
**Why:** GPG apt-signing key gets a passphrase (protects keyring + offline backup; CI still signs
non-interactively via loopback pinentry with GPG_PASSPHRASE secret — rejected passphrase-free for a key
whose leak would poison friends' apt trust). The AUR SSH key is the opposite call: dedicated,
passphrase-free, scope-limited deploy key, because the release workflow must push PKGBUILDs unattended
and the key can only push to our own AUR packages (web account controls stay password-guarded; AUR 2FA
verified nonexistent upstream as of today — enable when shipped). Secret VALUES live only in Raja's
password manager + GH Actions secrets; repo/docs record locations only.

## 2026-07-20 — The .deb is built on Ubuntu only; the build script fails loudly elsewhere
**Why:** cargo-deb's `$auto` dependency discovery runs dpkg-shlibdeps, which reads the built ELF
binaries and maps their SONAMEs and symbol-version floors (GLIBC, GTK) to versioned Ubuntu packages.
It exists only on Debian derivatives, and it is only *correct* when the binaries were compiled against
that distribution's libraries — a Fedora-built binary would encode a GLIBC floor Ubuntu can't satisfy.
Rejected: hardcoding the dependency list (drops real ABI floors, so a package installs then crashes at
startup) and silently building inside a container from `build-deb.sh` (hides the constraint and still
needs the container to do the compile). CI builds it on Ubuntu 24.10 in ticket 14.

## 2026-07-20 — Debian dev versions order on commit count, not committer timestamp
**Why:** dev versions are `<base>~git<count>.<ct>.<sha>-1`. The first ordering component must be
monotonic for any descendant commit. Committer timestamps are not: clock corrections, imported commits,
and explicit GIT_COMMITTER_DATE can make a child older than its parent, which would make apt refuse the
newer package as a downgrade. `git rev-list --count` is monotonic along history, so it leads; the
timestamp is a tiebreaker and the short SHA is an identifier only, never an ordering key. The count is
only trustworthy with full history, so shallow clones are refused outright. Rejected: SHA-based
ordering (random with respect to commit order — the original bug).

## 2026-07-20 — One RPM Release policy across dev, COPR snapshot, and tagged builds
**Why:** the dev path's `1.git<hex>` orders by random commit hash and outranks the first tagged
release `1`, so systems with a dev RPM would never upgrade to the real channel. Unified: every
pre-release build (dev + COPR snapshot) uses `0.<count>.<ct>.git<sha>` (count = full-history
`rev-list --count`, monotonic; timestamp tiebreaker), tagged releases use plain `N` from a
committed packaging value (respin = bump it). Cost: one-time `--oldpackage` downgrade on machines
with an installed `1.git<sha>` dev RPM (Raja's). Rejected: starting official releases at `2`
forever to outrank stale dev builds — pays a permanent numbering tax for a one-machine problem.

## 2026-07-20 — COPR tag builds self-pin in make-srpm.sh instead of via an API token
**Why:** the only COPR credential is the webhook URL (ticket 09); the generic webhook rebuild
clones mutable default-branch HEAD, so a tag push could build the wrong commit (tag-before-branch,
queued-job races). make-srpm.sh now fetches full history + tags, derives the version from cargo
metadata, and if `v<version>` exists checks out THAT commit to build the release; otherwise it's a
snapshot of HEAD. Rejected for now: COPR API-token pinned-ref builds (new secret + more surface) —
revisit in ticket 14 if the release workflow wants it.

## 2026-07-20 — COPR builds only the highest release tag; snapshots are a bootstrap-only mode
**Why:** the COPR webhook carries no commit, and deriving the build target from the clone's mutable
HEAD loses provenance (tag-before-branch pushes, queued-job races, tags on non-default branches).
make-srpm.sh now fetches tags fail-closed and builds the highest valid `v<semver>` tag's commit,
validating the tag name against that commit's own tree; a snapshot of HEAD is built only when no
release tag exists (pre-first-release). Queued races collapse to "latest release wins". Rejected:
per-tag pinned builds via the COPR API (needs a new API-token secret — ticket 14 may add it) and
deriving the tag from HEAD's version (the round-2 defect).

## 2026-07-20 — Apt channel: GitHub Pages self-hosted (apt-ftparchive + own GPG key)
**Why:** Ticket 09 already provisioned exactly this path (key, committed public keyring,
GPG_PRIVATE_KEY/GPG_PASSPHRASE secrets) while the Cloudsmith token was explicitly skipped; self-hosting
adds zero third-party accounts/approval risk and composes directly into ticket 14's on-tag workflow.
Rejected: Cloudsmith/packagecloud free OSS tier (signup + token + tier-approval dependency).
Corollaries: published package bytes are immutable (respin ⇒ version bump); support matrix pinned to
Ubuntu 26.04 amd64 until the Overlay's gtk4-layer-shell floor is resolved (24.04 lacks the package).

## 2026-07-20 — Mid-session secret lookup: bounded retry + a TTL-bounded session cache (GH #69)
**Why:** A warm daemon (hours of uptime) hit one transient D-Bus/ksecretd hiccup on the
per-Recording `secret-tool lookup` and hard-failed the whole dictation activation into the loud
env-var fallback. Live probing showed the incident surfaced through the empty-stderr arm: a genuine
"no such key" lookup is `exit 1` with EMPTY stderr, which Voisu classifies as definitive `Missing`
→ the default "Secret storage is unavailable; set VOISU_GROQ_API_KEY…" message (verbatim the message
in the issue). Two independent mechanisms, each fixing a distinct slice, without touching the
`secret-tool` boundary (no `keyring` crate — see 2026-07-20 above): **(1)** a small bounded retry on
the *transient-denial* arm only — a nonzero exit WITH a stderr diagnostic (indistinguishable by
output from a genuinely locked collection), budget `[100ms, 250ms]` ≈0.35s, honoring the existing
`VOISU_TEST_KEYRING_RETRY_MS` seam. The empty-stderr `Missing` arm stays definitive and is NEVER
retried, so the common unconfigured-key and 0600-file-fallback reads add zero latency; the happy path
(exit 0) never retries. This is the asymmetric opposite of the store path, where empty-stderr =
"still activating" = retryable. **(2)** a session-scoped, in-process credential cache (one slot per
provider, `Credential` has no `Debug`, never persisted or logged) so a credential loaded successfully
on an earlier Recording survives a later transient denial without re-shelling — the exact warm-daemon
failure mode. Env override still wins over the cache; `diagnose` (doctor) bypasses it for a live read.
**Invalidation is a TTL (300s), not a per-Recording 401/403 hook:** the hot transcription path runs
curl with `--fail`, which collapses every 4xx into a generic `Provider` error with no recoverable
status, so there is no clean in-daemon "the provider rejected this key" signal; surfacing one would
mean de-hardening the security-reviewed Groq/Deepgram request builders (disproportionate blast radius
for this fix). The TTL bounds how long a mid-session key rotation is served stale (the daemon re-reads
once the entry expires) and comfortably outlasts any transient hiccup. An unused `invalidate()` method
is kept as the seam for a future auth-rejection hook. Rejected: retrying the empty-stderr `Missing`
arm (penalizes every file-fallback Recording); wiring 401/403 invalidation now (blast radius). Full
retry/cache semantics and budgets live in `crates/voisu-app/src/system.rs`.
