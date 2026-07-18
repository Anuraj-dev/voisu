# Voisu — State
> Cloud-first Linux desktop dictation app (Fedora KDE Plasma / Wayland) · Last checkpoint: 2026-07-18

## 🚧 In progress / next
- **BOTH CRITICAL audit fixes SHIPPED (PRs #25 + #26, merged, CI green).**
  Next frontier: the **latency effort** (`.scratch/voisu-latency/` +
  `docs/specs/2026-07-17-latency-optimization.md`) — prune its "delivery fix"
  item first (done in PR #24). Hardening tickets 03 (systemd hardening) and
  04 (CI audit/clippy gate) are parallel-safe anytime; 05 hygiene waits
  behind latency.
- **Live RPM ship gate deliberately SKIPPED this session** — the criticals
  change failure-path behavior + thread placement only; happy path is pinned
  by the new single-worker responsiveness regression + 3x CI flake gate. The
  next RPM build carries both fixes (installed RPM is still `gitfd3c663`).
- **New dispatch channel available: `cladex`** (`~/.local/bin/claude-codex`) —
  runs gpt-5.6-* inside the Claude Code harness via local CLIProxyAPI
  (port 8317). Verified end-to-end incl. `--effort` passthrough (wire-checked).
  Raja's standing instruction (2026-07-18): use cladex for Sol dispatches, and
  split work ~50/50 Claude (Opus/driver) vs Codex — Sol keeps architecture-
  grade impl + all reviews; Opus takes scoped lifecycle fixes/point work/docs.
- Future idea (no ticket): packaging beyond RPM (Arch PKGBUILD etc.), after
  latency. Priority 2 unchanged: Overlay visual polish.
- Deferred release acceptance still untested: logout/login startup, kill-
  Overlay-mid-Recording, clean uninstall.

## Status
- **C1 FIXED (PR #25, merge `da279b3`):** `process_recording` is supervised by
  `supervise_recording` mirroring `supervise_replay` — a capture-pump or
  processing panic becomes a failed Recording (honest diagnostics, rebuilt
  stateless adapters, reaper drained) and the actor ALWAYS returns to Idle.
  Hardened through 4 review rounds: poison-tolerant `DiagnosticStore` locks,
  no new persisted enum variants (rollback-safe), configured-provider-only
  panic accounting, `log_best_effort` (never `eprintln!`) on guaranteed-
  completion paths.
- **C2 FIXED (PR #26, merge `c940d65`):** `stop_child` busy-poll moved to
  `spawn_blocking` (`stop_child_blocking`, body/classification unchanged).
  Cancellation-safe via reaper adoption: cancelled cleanup handles AND
  pre-stop capture drops are retained in the actor-owned `ProviderReaper`
  (`adopt_capture_blocking`, poison-tolerant `retain()`); all Idle-permitting
  paths call `drain_to_completion`. Next Recording can never overlap the
  previous Recording's pw-record cleanup.
- Test baseline: **307** (was 300; +7 across both fixes). One flaky-looking
  local failure post-merge was rebuild-contention noise: 2 full clean runs +
  lifecycle suite 3x green + CI 3x gate green.
- Delivery bug (PR #24) + accuracy (PR #23, WER 9.2%) merged earlier same day.
- `docs/model-benchmark.md` rows 89–102 logged (H1/H2 cladex session).

## Architecture map
- Domain, IPC, Transcript decision, diagnostics -> `crates/voisu-core/src/lib.rs`, `diagnostics.rs`
- Fedora capture/provider/clipboard/portal/libei adapters + ProviderReaper -> `crates/voisu-app/src/system.rs`
- Recording/replay supervision (`supervise_recording`/`supervise_replay`) -> `crates/voisu-app/src/bin/voisu-daemon.rs`
- Dictionary / Whisper prompt builder -> `crates/voisu-app/src/dictionary.rs`
- Daemon + Overlay user-service lifecycle -> `crates/voisu-app/src/service.rs`
- Public CLI -> `crates/voisu-app/src/bin/voisu.rs`
- GTK Overlay -> `crates/voisu-app/src/bin/voisu-overlay.rs`, `overlay.rs`, `feedback.rs`
- RPM units/spec/build/smoke -> `packaging/`
- Accuracy effort -> `docs/specs/2026-07-17-transcription-accuracy.md`, `.scratch/voisu-accuracy/`
- Latency effort -> `docs/specs/2026-07-17-latency-optimization.md`, `.scratch/voisu-latency/`
- Hardening map + audit -> `.scratch/voisu-hardening/` (01+02 CLOSED)
- Fedora procedure/evidence -> `docs/packaging-fedora.md`, `docs/release-evidence.md`

## Stack & run
- Stack: Rust 2024 + Tokio + serde + zbus 5 + GTK4 (opt-in) + runtime libei · Run: `cargo run -p voisu-app --bin voisu-daemon` · Test: `cargo test --workspace`
- Robust test count: `cargo test --workspace 2>&1 | grep -oE "[0-9]+ passed; [0-9]+ failed" | awk '{p+=$1; f+=$3} END {print p, f}'`
- Test baseline: **307**.

## Key decisions (top 3–5)
- Guaranteed-completion supervisor paths (anything between the supervised
  `.await` and the actor send) must be panic-FREE: poison-tolerant locks,
  `log_best_effort` not `eprintln!`, no new persisted enum discriminators
  (old binary's `load_raw` wipes history on unknown variants).
- Cleanup exclusion via reaper: Idle is permitted only after
  `drain_to_completion` — cancelled/dropped capture and provider cleanup is
  adopted, never detached.
- EIS/portal keymap fds: `pread` at offset 0, never the shared cursor.
- Never arm PR_SET_PDEATHSIG from a transient thread.
- rustls = ring; Groq primary + Deepgram second opinion (disableable — toggle
  still a queued latency ticket, daemon always configures both today).

## Gotchas
- **Disk critically tight (~11–14 GB).** `cargo clean` before RPM builds;
  `TMPDIR=/var/tmp RUST_TEST_THREADS=4`; build script refuses a dirty tree.
- cladex gotchas: prompt via **stdin** when combining `-p` with other flags;
  headless needs `--permission-mode acceptEdits` + explicit
  `--allowedTools 'Bash(cargo:*)'`; costs in JSON are nominal (real billing =
  Codex Plus quota); proxy auto-starts/stops per session (refcounted).
- Shared-CARGO_TARGET_DIR parallel worktrees work but cause rebuild churn —
  expect one-off timing-test flakes under contention; re-run before panicking.
- `packaging/build-rpm.sh` requires a clean COMMITTED checkout; embeds commit.
- Whisper `prompt` ~224 tokens; Groq free tier: 7,200 audio-sec/hr, 2,000 req/day.
- Use `CONTEXT.md` terms exactly; default builds GTK-free (`--features overlay`).
  `rustfmt`/`clippy` unavailable.
- Leftover diagnostics (optional cleanup): `/var/tmp/pwtest.raw`,
  `/var/tmp/pwpipe.err`, fixture `pwtest.raw` under
  `/run/user/1000/voisu/v1/diagnostics/fixtures/`.
