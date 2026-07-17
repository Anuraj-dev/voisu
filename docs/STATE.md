# Voisu — State
> Cloud-first Linux desktop dictation app (Fedora KDE Plasma / Wayland) · Last checkpoint: 2026-07-17

## 🚧 In progress / next
- **Transcription-accuracy effort is CODE-COMPLETE and fully integrated** on branch
  `feature/transcription-accuracy` (branch tip `15c82f9`, docs-only; NOT pushed
  anywhere). All three tickets closed with Sol APPROVE and merged onto the branch.
  296 workspace tests passing, 0 failed; overlay check clean.
  **Blocked only on the live-test gate** (RPM build + live WER dictation).
- **BLOCKED — RPM + live WER gate (per the orchestration plan):**
  1. RPM build via `packaging/build-rpm.sh` — failed twice on "Disk quota
     exceeded" (os error 122) in `%check` temp dirs (NOT code; `service_cli`
     passes 30/30 locally). Retry in progress after driver freed ~11 GB (removed
     merged worktrees `agent-ac07d292893b98ce5` + `ticket05-deepgram-streaming`,
     ran `cargo clean`). An RPM build is running concurrently in this checkout.
  2. On build success: `sudo dnf install dist/rpm/voisu-0.1.0-1.git15c82f9*.rpm`
     (also `voisu-overlay`), restart `voisu.service`, verify `voisu doctor`/auth.
  3. Raja dictates the 4 Appendix A paragraphs
     (`docs/specs/2026-07-17-transcription-accuracy.md`) live; score WER per
     source + final (normalized, punctuation-insensitive; baseline 26.3%). Pass:
     overall ≤10%, no fluent-nonsense substitutions, no silent provider absence,
     release-to-text ≤~1 s after finalize.
  4. Only if pass: push branch, open PR, merge on CI green. If fail: report
     scored evidence and STOP.
- **PRIORITY ORDER after this branch integrates to main (decided 2026-07-18):**
  land the TWO CRITICAL audit fixes FIRST, before any latency ticket — (1)
  supervise `process_recording`/remove `pump.await.expect` (voisu-daemon.rs:1396,
  bare spawn at :577 — panic wedges daemon in Processing forever; mirror
  `supervise_replay`), (2) wrap blocking `stop_child` in `spawn_blocking`
  (system.rs:1476/1488 — up to ~2 s worker-thread stall per stop). Both touch the
  same files as latency tickets 01 & 04; criticals land first so latency rebases
  over small diffs. Full audit backlog is being charted in the hardening
  wayfinder map (rest of it waits BEHIND latency).
- **Latency effort queued behind this branch** (separate map `.scratch/voisu-latency/`
  + plan `docs/specs/2026-07-17-latency-optimization.md`; decisions D1–D4 in
  `decisions.md`). Do NOT start until `feature/transcription-accuracy` integrates
  to main — tickets 01 & 04 touch the same `system.rs`/`lib.rs`/daemon files.
- Priority 2 unchanged: Overlay visual redesign/polish (functional v1 is in).
- Deferred release acceptance still untested: logout/login startup observation,
  kill-Overlay-mid-Recording, clean uninstall.

## Status
- **Ticket 04 (Groq accuracy): CLOSED, Sol APPROVE.** Merged via `1503d26`.
  `dictionary.rs` (`merged_terms()`, `whisper_prompt()`, 224-token budget),
  Whisper prompt + `language=en` + `temperature=0`, `VOISU_GROQ_MODEL` override
  (default `whisper-large-v3`), ≤120 s full-audio finalize else 60 s chunks.
- **Ticket 05 (Deepgram nova-3 websocket streaming): CLOSED, Sol APPROVE**, 0
  findings on fix `abf4fd9` (redial redesigned: `audio_delivered` gate — loss
  after delivered audio fails visibly, no redial; drain requires terminal
  Metadata evidence; ws userinfo structurally rejected). Merged via `30ee55e`;
  driver resolved 3 conflicts (kept 04's full-audio/60 s-chunk Groq constants +
  `build_groq_curl_config`, deleted retired batch Deepgram path, kept both
  lifecycle tests) and wired `dictionary::merged_terms()` into
  `DeepgramProvider::with_keyterms` at both daemon construction sites.
- **Ticket 06 (divergence gate + §3.5 visibility): CLOSED, Sol APPROVE at
  `b2b83a0`** after EIGHT review rounds. Accepted design (`4f71124`): single
  symmetric `phonetic_matching` feeds gate + selection; garbage-asymmetry /
  fragment / agreement<0.2 tiers; low-confidence §3.5 annotation when intrinsic
  tiers decide. Final fix `b2b83a0`: hollow floor aligned to
  `CONTENT_OVERLAP_FLOOR` 0.2. Accepted residuals: sea/see ≤3-char exact rule;
  4-distinct pure-nonsense loop reconciles.
- **NEW live bug found & fixed (`b7b01a4`):** every Recording was killed at
  exactly 60 s (default `VOISU_RECORDING_DEADLINE_MS` fallback,
  journalctl-proven). Default now 600 s via pure `resolve_recording_deadline`
  seam. Chunking was dead code without this (PRD assumed >120 s recordings).
- Accuracy diagnosis (2026-07-17 blind test, 26.3% WER): root causes = Deepgram
  1 s batch-chunk design, Groq without prompt/language, 30 s chunk seams.
  Reconciliation was NOT the villain (refuted).
- Overlay graphical startup merged locally (`67aa3b6`); local `main` ahead of
  `origin/main`, not pushed.
- `docs/model-benchmark.md` rows 61–83 complete (committed).

## Architecture map
- Domain, IPC, Transcript decision, diagnostics -> `crates/voisu-core/src/lib.rs`
- Fedora capture/provider/clipboard/portal/libei adapters -> `crates/voisu-app/src/system.rs`
- Dictionary / Whisper prompt builder (ticket 04) -> `crates/voisu-app/src/dictionary.rs`
- Daemon + Overlay user-service lifecycle -> `crates/voisu-app/src/service.rs`
- Public CLI -> `crates/voisu-app/src/bin/voisu.rs`
- GTK Overlay -> `crates/voisu-app/src/bin/voisu-overlay.rs`, `overlay.rs`, `feedback.rs`
- RPM units/spec/build/smoke -> `packaging/`
- Accuracy effort -> `docs/specs/2026-07-17-transcription-accuracy.md` (PRD),
  `.scratch/voisu-accuracy/` (map + tickets + pricing asset)
- Latency effort -> `docs/specs/2026-07-17-latency-optimization.md` (plan),
  `.scratch/voisu-latency/` (map + tickets)
- Fedora procedure/evidence -> `docs/packaging-fedora.md`, `docs/release-evidence.md`

## Stack & run
- Stack: Rust 2024 + Tokio + serde + zbus 5 + GTK4 (opt-in) + runtime libei · Run: `cargo run -p voisu-app --bin voisu-daemon` · Test: `cargo test --workspace`
- Robust test count: `cargo test --workspace 2>&1 | grep -oE "[0-9]+ passed; [0-9]+ failed" | awk '{p+=$1; f+=$3} END {print p, f}'`

## Key decisions (top 3–5)
- Transcription accuracy overhaul per PRD: Groq single-request + vocabulary
  prompt is the primary accuracy lever; Deepgram = real nova-3 streaming second
  opinion, cleanly disableable (only non-free component).
- Acceptance bar: ≤10% WER on the live 4-paragraph technical suite, no
  fluent-nonsense substitutions, no silent provider absence, latency ≤ today.
- Reconciliation gated on source comparability (divergence gate) so a degenerate
  source cannot poison the final Transcript.
- Three-strike subagent escalation rule: 3 failed review rounds → discard the
  agent, respawn fresh at higher effort with the findings history (proven on
  ticket 06; full entry in `decisions.md`).
- Portals are the normal Fedora path; no raw input devices or `uinput`.

## Gotchas
- **Disk critically tight (~14 GB free).** `rpmbuild` needs workspace debug +
  release trees PLUS its own `%check` build tree — `cargo clean` target/ before
  RPM builds. Build script refuses a dirty status (untracked files count):
  `.git/info/exclude` now locally ignores `.claude/`, `.scratch/voisu-latency/`,
  `docs/specs/2026-07-17-latency-optimization.md`.
- **Keyring:** "secret service lookup denied" seen once in logs (19:06) — verify
  keyring unlocked + `voisu auth verify` before the live test.
- **Parallel branch `fix/delivery-keymap-fd`** (keymap fd pread fix) lives in a
  SEPARATE worktree (`/tmp`), untouched; not part of this branch. Accuracy work
  must not touch `system.rs` `keyboard_keymap_text`/libei region ~3033–3110.
- Latency-optimization effort (other session, decisions D1–D4) is sequenced
  AFTER this branch integrates.
- Use `CONTEXT.md` terms exactly; ordinary synonyms are intentionally banned.
- Default workspace builds are GTK-free; Overlay needs `--features overlay`.
- `packaging/build-rpm.sh` requires a clean COMMITTED checkout (push not
  required) and embeds the commit.
- Whisper `prompt` honors only ~224 tokens — dictionary builder budgets it.
- Groq free-tier limits (7,200 audio-sec/hr, 2,000 req/day) are the rate ceiling.
- `rustfmt` and `clippy` are unavailable.
- Delivery still uses `clipboard_fallback` (`xkbcommon` parse errors) — Raja
  pastes manually. Tracked as **latency ticket 05** (`fix/delivery-keymap-fd`);
  do NOT fold into the accuracy branch.
