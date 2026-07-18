# Voisu — State
> Cloud-first Linux desktop dictation app (Fedora KDE Plasma / Wayland) · Last checkpoint: 2026-07-18

## 🚧 In progress / next
- **Accuracy branch LIVE-TESTED — awaiting Raja's ship/hold decision.** Branch
  `feature/transcription-accuracy` tip `2f90a10` (NOT pushed). RPM
  `git2f90a10` built, installed, and live-verified on this machine.
  Live WER (4-paragraph Appendix A suite, baseline 26.3%): **final overall
  10.8% raw / 10.0% formatting-adjusted** — P1 6.1%, P2 (CLI vocab) 19.3%,
  P3 6.0%, P4 12.9%. Both providers present on every recording; post-finalize
  latency 1.0–1.5 s (bar ~1 s); stress test **118 s continuous recording PASS**.
  Residual fluent-nonsense on rare CLI/domain terms ("rpmbuild"→"RPM build",
  "changelog"→"channel log", "reconciler"→"Reconcealer"). Gate is MARGINAL,
  not a clean pass → per rules, NOT pushed/PR'd.
  **Next: Raja decides — (a) ship as-is (26.3%→~10% stands), then push branch,
  PR to main, merge on CI green (no AI credits); or (b) one more accuracy pass
  wiring CLI jargon into dictionary keyterms before shipping.**
- **PRIORITY ORDER after this branch integrates to main (decided 2026-07-18):**
  land the TWO CRITICAL audit fixes FIRST, before any latency ticket — (1)
  supervise `process_recording`/remove `pump.await.expect` (voisu-daemon.rs,
  panic wedges daemon in Processing forever; mirror `supervise_replay`), (2)
  wrap blocking `stop_child` in `spawn_blocking`. Backlog charted in
  `.scratch/voisu-hardening/` (03 systemd-hardening + 04 CI-audit/clippy are
  parallel-safe anytime; 05 hygiene waits behind latency).
- **Latency effort queued behind this branch** (`.scratch/voisu-latency/` +
  `docs/specs/2026-07-17-latency-optimization.md`). Do NOT start until this
  branch integrates — tickets 01 & 04 touch the same files.
- Priority 2 unchanged: Overlay visual redesign/polish (functional v1 is in).
- Deferred release acceptance still untested: logout/login startup observation,
  kill-Overlay-mid-Recording, clean uninstall.

## Status
- **Two live-blocking daemon bugs found & fixed this session (2026-07-18):**
  1. `a0899b3` — rustls 0.23 had no process-level CryptoProvider (tungstenite
     `rustls-tls-webpki-roots` picks no backend) → every Deepgram connect
     panicked. Fixed: explicit `rustls/ring` dep + `install_crypto_provider()`
     at daemon startup (ring not aws-lc-rs: no cmake, vendored RPM builds).
  2. `f04dbbe` — **recordings died at exactly ~10 s**: PR_SET_PDEATHSIG fires
     when the forking THREAD exits; pw-record was spawned from a Tokio
     blocking-pool thread reaped after its 10 s idle keep-alive → SIGKILL
     mid-recording. Fixed: spawn pw-record FROM the capture reader thread
     (mpsc handoff). Regression test proven RED→GREEN
     (`recording_survives_blocking_pool_thread_reap`, pins /proc starttime +
     non-zombie + status Recording).
  Plus `99d0f9e`/`1a63b72` (%check test races: atomic pid writes, ETXTBSY
  retry) and `0615736` (Sol findings: RPM license `MIT AND Apache-2.0 AND ISC`
  + ring license texts shipped; regression-test false-green hardened).
  298 tests green ×3. Sol high review → 2 findings → fixed → Sol medium APPROVE.
- Live pipeline verified end-to-end: all 6 stages complete, reconciliation
  beats both sources (P1–P3), delivery via clipboard_fallback (keymap fd bug,
  separate branch). Stress: 118.2 s, 1,846 chunks, both providers.
- Tickets 04/05/06 all CLOSED with Sol APPROVE (details in decisions.md +
  session logs; merges `1503d26`, `30ee55e`, `b2b83a0`).
- Full codebase audit done 2026-07-18; 2 criticals queued (see above); report
  in `.scratch/voisu-hardening/`.
- Overlay graphical startup merged locally (`67aa3b6`); local `main` ahead of
  `origin/main`, not pushed.
- `docs/model-benchmark.md` rows 61–86 complete (committed).

## Architecture map
- Domain, IPC, Transcript decision, diagnostics -> `crates/voisu-core/src/lib.rs`
- Fedora capture/provider/clipboard/portal/libei adapters -> `crates/voisu-app/src/system.rs`
- Dictionary / Whisper prompt builder -> `crates/voisu-app/src/dictionary.rs`
- Daemon + Overlay user-service lifecycle -> `crates/voisu-app/src/service.rs`
- Public CLI -> `crates/voisu-app/src/bin/voisu.rs`
- GTK Overlay -> `crates/voisu-app/src/bin/voisu-overlay.rs`, `overlay.rs`, `feedback.rs`
- RPM units/spec/build/smoke -> `packaging/`
- Deepgram real-time diagnostic probe -> `crates/voisu-app/examples/deepgram_probe.rs`
- Accuracy effort -> `docs/specs/2026-07-17-transcription-accuracy.md` (PRD + Appendix A refs),
  `.scratch/voisu-accuracy/`
- Latency effort -> `docs/specs/2026-07-17-latency-optimization.md`, `.scratch/voisu-latency/`
- Fedora procedure/evidence -> `docs/packaging-fedora.md`, `docs/release-evidence.md`

## Stack & run
- Stack: Rust 2024 + Tokio + serde + zbus 5 + GTK4 (opt-in) + runtime libei · Run: `cargo run -p voisu-app --bin voisu-daemon` · Test: `cargo test --workspace`
- Robust test count: `cargo test --workspace 2>&1 | grep -oE "[0-9]+ passed; [0-9]+ failed" | awk '{p+=$1; f+=$3} END {print p, f}'`

## Key decisions (top 3–5)
- Never arm PR_SET_PDEATHSIG from a transient thread: pdeathsig fires on the
  forking THREAD's exit — external children must be spawned from a thread that
  outlives them (capture reader thread), never the Tokio blocking pool.
- rustls crypto backend = ring (explicit install at startup); aws-lc-rs
  rejected (cmake dep breaks vendored RPM builds).
- Acceptance bar: ≤10% WER on the live 4-paragraph suite, no fluent-nonsense,
  no silent provider absence, latency ≤ today. Live result is marginal — ship
  decision is Raja's, not auto-merge.
- Transcription accuracy overhaul per PRD: Groq single-request + vocabulary
  prompt primary; Deepgram nova-3 streaming second opinion, disableable.
- Portals are the normal Fedora path; no raw input devices or `uinput`.

## Gotchas
- **Disk critically tight (~14 GB).** `cargo clean` before RPM builds; build
  needs `TMPDIR=/var/tmp RUST_TEST_THREADS=4`; script refuses a dirty tree
  (untracked count; `.git/info/exclude` locally ignores `.claude/`,
  `.scratch/voisu-latency/`, latency spec).
- **Parallel branch `fix/delivery-keymap-fd`** in a separate worktree —
  untouched; don't touch `system.rs` keymap/libei region ~3033–3110.
- Delivery still `clipboard_fallback` (xkbcommon parse errors) — Raja pastes
  manually; fix lives on that separate branch, NOT here.
- Regression test uses `VOISU_TEST_BLOCKING_KEEP_ALIVE_MS` seam to shrink the
  blocking-pool keep-alive; don't remove the env hook from voisu-daemon main.
- `packaging/build-rpm.sh` requires a clean COMMITTED checkout; embeds commit.
- Whisper `prompt` honors only ~224 tokens; Groq free tier: 7,200 audio-sec/hr,
  2,000 req/day.
- Use `CONTEXT.md` terms exactly; default builds GTK-free (`--features overlay`
  for Overlay). `rustfmt`/`clippy` unavailable.
- Leftover diagnostics (optional cleanup): `/var/tmp/pwtest.raw`,
  `/var/tmp/pwpipe.err`, fixture `pwtest.raw` under
  `/run/user/1000/voisu/v1/diagnostics/fixtures/`.
