# Voisu — State
> Cloud-first Linux desktop dictation app (Fedora KDE Plasma / Wayland) · Last checkpoint: 2026-07-18

## 🚧 In progress / next
- **Latency effort: both AFK tickets SHIPPED.** L-01 Deepgram default-OFF toggle
  (PR #27) and L-04 FLAC upload (PR #28) merged, CI green. Ticket 05 (delivery)
  was closed as already-shipped (PR #24). Remaining latency tickets are
  **HITL-gated on Raja**:
  - **Ticket 02** — live Groq-only evaluation (Raja dictates the 4-paragraph
    suite + everyday commands with Deepgram OFF; capture tail + proper-noun
    quality to `.scratch/voisu-latency/assets/02-groq-only-evidence.md`).
  - **Ticket 03** — Deepgram fate decision (keep-as-opt-in vs delete), after 02.
  - **RPM ship gate** (runs ONCE, end of effort): TMPDIR=/var/tmp
    RUST_TEST_THREADS=4, clean committed tree, cargo clean before/after,
    ~11–14 GB disk; live latency measurements + WER suite re-run (FLAC is
    lossless — expect zero change) as evidence per the spec. Installed RPM is
    still `gitfd3c663` (pre-criticals, pre-latency).
- Hardening tickets 03 (systemd hardening) + 04 (CI audit/clippy gate) remain
  parallel-safe anytime; 05 hygiene waits behind latency.
- Priority 2 unchanged: Overlay visual polish. Future idea: packaging beyond RPM.

## Status
- **L-01 (PR #27):** `voisu deepgram on|off` persisted in
  `$XDG_CONFIG_HOME/voisu/config.toml`, default OFF → Groq-only fast path
  (~690 ms tail vs ~1889 ms reconciled). `VOISU_DISABLE_DEEPGRAM` env override.
  Disabled = `DisabledProvider` no-network stand-in (no stream, no credential
  load, barrier waits on Groq alone, no reconciliation). Canonical diagnostic
  "Deepgram disabled for this Recording" recorded NotStarted on EVERY record
  path. Keyterm snapshot resolved once at startup (replay tail does zero fs).
  Atomic config writes; unreadable config never destructively replaced.
- **L-04 (PR #28):** Groq uploads FLAC (pure-Rust `flacenc`, default-features
  off) instead of raw WAV — ~42% payload cut, ~3 ms short-clip encode → no
  duration gate. Curl sandbox byte-identical. Deepgram untouched (streams
  linear16 WS; ticket's assumed batch path didn't exist).
- Test baseline: **330** (307 → +21 L-01 → +2 L-04). One CI failure mid-effort:
  FLAC test assertions raced the fake pw-record's post-signal trap bytes —
  fixed by pinning bounds to the deterministic pre-stop capture.
- `docs/model-benchmark.md` rows 103–113: Sol/Opus head-to-head (both roles,
  alternating; double review on L-04) + updated routing recommendation.

## Architecture map
- Domain, IPC, Transcript decision, diagnostics -> `crates/voisu-core/src/lib.rs`, `diagnostics.rs`
- Fedora capture/provider/clipboard/portal/libei adapters + ProviderReaper + FLAC encode -> `crates/voisu-app/src/system.rs`
- Recording/replay supervision + DisabledProvider -> `crates/voisu-app/src/bin/voisu-daemon.rs`
- Persisted config (deepgram toggle) -> `crates/voisu-app/src/config.rs`
- Dictionary / Whisper prompt builder -> `crates/voisu-app/src/dictionary.rs`
- Daemon + Overlay user-service lifecycle -> `crates/voisu-app/src/service.rs`
- Public CLI (incl. `voisu deepgram`) -> `crates/voisu-app/src/bin/voisu.rs`
- GTK Overlay -> `crates/voisu-app/src/bin/voisu-overlay.rs`, `overlay.rs`, `feedback.rs`
- RPM units/spec/build/smoke -> `packaging/`
- Latency effort -> `docs/specs/2026-07-17-latency-optimization.md`, `.scratch/voisu-latency/`
- Accuracy effort -> `docs/specs/2026-07-17-transcription-accuracy.md`, `.scratch/voisu-accuracy/`
- Hardening map + audit -> `.scratch/voisu-hardening/` (01+02 CLOSED)
- Fedora procedure/evidence -> `docs/packaging-fedora.md`, `docs/release-evidence.md`

## Stack & run
- Stack: Rust 2024 + Tokio + serde + zbus 5 + GTK4 (opt-in) + runtime libei + flacenc · Run: `cargo run -p voisu-app --bin voisu-daemon` · Test: `cargo test --workspace`
- Robust test count: `cargo test --workspace 2>&1 | grep -oE "[0-9]+ passed; [0-9]+ failed" | awk '{p+=$1; f+=$3} END {print p, f}'`
- Test baseline: **330**.

## Key decisions (top 3–5)
- Deepgram = default-OFF runtime toggle (delete-or-keep finalized by ticket 03
  after live eval); disabled Provider is an adapter stand-in, not coordinator
  surgery — supervision/reaper/barrier untouched.
- FLAC (lossless) upload, no duration gate (3 ms short-clip encode measured);
  Opus codec rejected for WER risk.
- Test assertions must pin only deterministic pre-stop capture — post-signal
  bytes are best-effort by design (stop adopts capture into the reaper).
- Guaranteed-completion supervisor paths stay panic-FREE and fs-FREE (config +
  keyterms resolved once at daemon start).
- Never require raw input-device/privileged uinput access on the Fedora path.

## Gotchas
- **Disk critically tight (~11–14 GB).** `cargo clean` before RPM builds;
  `TMPDIR=/var/tmp RUST_TEST_THREADS=4`; build script refuses a dirty tree.
- cladex gotchas: prompt via **stdin** when combining `-p` with other flags;
  headless needs `--permission-mode acceptEdits` + explicit `--allowedTools`;
  costs in JSON are nominal (real billing = Codex Plus quota).
- **Subagent doc fence:** both Sol and Opus coders scope-creeped into
  orchestrator-owned docs (STATE/benchmark) once each despite doc-skip
  instructions — every dispatch prompt needs an explicit "do not touch
  docs/STATE/checkpoint/benchmark" fence, and diffs must be checked for it.
- The accuracy WER suite assumes Deepgram ON — run it with `voisu deepgram on`.
- Shared-CARGO_TARGET_DIR parallel worktrees cause rebuild churn — expect
  one-off timing-test flakes under contention; re-run before panicking.
- `packaging/build-rpm.sh` requires a clean COMMITTED checkout; embeds commit.
- Whisper `prompt` ~224 tokens; Groq free tier: 7,200 audio-sec/hr, 2,000 req/day.
- Use `CONTEXT.md` terms exactly; default builds GTK-free (`--features overlay`).
  `rustfmt`/`clippy` unavailable.
- Leftover diagnostics (optional cleanup): `/var/tmp/pwtest.raw`,
  `/var/tmp/pwpipe.err`, fixture `pwtest.raw` under
  `/run/user/1000/voisu/v1/diagnostics/fixtures/`.
