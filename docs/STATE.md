# Voisu — State
> Cloud-first Linux desktop dictation app (Fedora KDE Plasma / Wayland) · Last checkpoint: 2026-07-17

## 🚧 In progress / next
- **Executing the transcription-accuracy PRD** on branch
  `feature/transcription-accuracy` (off local `main` `67aa3b6`, NOT pushed).
  PRD + tickets committed `ee76028`
  (`docs/specs/2026-07-17-transcription-accuracy.md`, `.scratch/voisu-accuracy/`).
  Three implementers ran in parallel; state per ticket below.
- **Ticket 04 — Groq accuracy (Opus): CLOSED, Sol APPROVE** (1 fix round).
  In worktree `.claude/worktrees/agent-ac07d292893b98ce5`
  (branch `worktree-agent-ac07d292893b98ce5`), commits `19cd716` + `239ef1a`,
  base `074da5f` (NOT the feature tip — integration must rebase/merge). Delivered:
  `crates/voisu-app/src/dictionary.rs` (`merged_terms()`, `whisper_prompt()`,
  224-token budget), Whisper prompt + `language=en` + `temperature=0`,
  `VOISU_GROQ_MODEL` override (default `whisper-large-v3`), ≤120 s full-audio
  finalize else 60 s chunks / 4 s overlap / 48-word dedup. 238 tests green there.
- **Ticket 05 — Deepgram nova-3 websocket streaming (Fable): FIX ROUND IN PROGRESS.**
  Worktree `.claude/worktrees/ticket05-deepgram-streaming`
  (branch `ticket05-deepgram-streaming`, base `ee76028`), commit `132f225`.
  tokio-tungstenite 0.24 rustls, `TranscriptAccumulator` (is_final only),
  keyterm seam `DeepgramProvider::with_keyterms(reaper, Vec<String>)` — daemon
  call sites still `::new`; driver wires `dictionary::merged_terms()` at
  integration (`voisu-daemon.rs:391` and `:1524`). 229 tests were green. Sol
  returned 7 findings (1 BLOCKER: reconnect silently drops unfinalized audio;
  2 HIGH: drain treats truncation as success + `ws://` userinfo token leak;
  3 MEDIUM, 1 LOW). Agent resumed 2026-07-17 evening.
- **Ticket 06 — reconciliation divergence gate + provider-failure visibility
  (Opus): ROUND-2 FIX IN PROGRESS.** Commits `54e29ff` + `d63b8a4` directly on
  `feature/transcription-accuracy`. Divergence gate in
  `TranscriptDecisionPipeline::decide`, `ProviderFailure`/`ProviderFailureStage`
  records, diagnostics scrubbing. 238 tests were green. Sol round-2 returned 6
  HIGH (Groq bias in `clean_source_fallback`; quality-score gaming by
  unique-word salad + `is_degenerate` false-positives on jargon; winner
  transcript erased on loser-cleanup failure `lib.rs:1438`; visibility gaps
  `voisu-daemon.rs:755/634/1364`; URL-scrub misses uppercase/ws/wss
  `diagnostics.rs:477`; unscrubbed `delivery_fallback_reason` `diagnostics.rs:525`).
  Agent resumed.
- **Next after fix rounds:** Sol re-reviews (medium) to APPROVE per ticket →
  integrate on the feature branch (merge 04 + 05, resolve `system.rs` /
  `daemon_cli_lifecycle.rs` overlaps, wire keyterms) → full gates
  (`cargo test --workspace` + `cargo check -p voisu-app --features overlay`) →
  local RPM build + Raja's live 4-paragraph Appendix A dictation, WER vs 26.3%
  baseline, pass bar ≤10% overall + latency ≤~1 s → push/PR/merge ONLY on pass.
- **Latency effort CHARTED (2026-07-17), queued behind accuracy integration.**
  Separate map `.scratch/voisu-latency/` + plan
  `docs/specs/2026-07-17-latency-optimization.md`. Evidence: `voisu history` recs
  20–39 → tail ~1889 ms reconciled vs ~690 ms Groq-only (~400 ms floor); Deepgram
  gates the barrier 12/12 and its 282 ms RTT is structural. Locked decisions:
  (D1) Deepgram → default-off `voisu deepgram on|off` toggle, evaluate live then
  finalize delete-vs-keep; (D2) keep curl, defer TLS warm-up; (D3) FLAC not Opus;
  (D4) fix direct-typing delivery, auto-paste as fallback. **Do NOT start
  implementation until `feature/transcription-accuracy` integrates to main** —
  tickets 01 & 04 touch the same `system.rs`/`lib.rs`/daemon files. Frontier
  tickets when unblocked: 01 (toggle), 04 (FLAC), 05 (delivery, own branch).
- Priority 2 unchanged: Overlay visual redesign/polish (functional v1 is in).
- Deferred release acceptance still untested: logout/login startup
  observation, kill-Overlay-mid-Recording, clean uninstall.

## Status
- Accuracy diagnosis DONE (2026-07-17 blind test, 26.3% WER): root causes =
  Deepgram 1 s batch-chunk design (word salad / silent absence), Groq without
  prompt/language (jargon errors), 30 s chunk seams. Reconciliation was NOT the
  villain (refuted).
- Overlay graphical startup merged locally (`67aa3b6`); local `main` ahead of
  `origin/main`, not pushed. Packaged units installed and verified live.
- Accuracy PRD bound to research: Deepgram streaming guide (Raja's vault:
  `AI Created Stuff/Hyprvox Rebuild/Deepgram Streaming Guide for Voisu.md`) +
  pricing asset (`.scratch/voisu-accuracy/assets/02-groq-deepgram-pricing.md`).
- `docs/model-benchmark.md` rows 61–67 track this effort's dispatches
  (uncommitted).

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
- Overlay presentation stays observer-only and disposable; keep GTK4 +
  gtk4-layer-shell for the KWin layer-shell surface.
- Portals are the normal Fedora path; no raw input devices or `uinput`.

## Gotchas
- **Target dirs deleted 2026-07-17** (Raja freed disk) — full rebuilds expected;
  keep disk usage frugal, clean up worktrees after integration.
- **Parallel branch `fix/delivery-keymap-fd`** (keymap fd pread fix) lives in a
  SEPARATE worktree, NOT part of this branch; accuracy agents must not touch
  `system.rs` `keyboard_keymap_text`/libei region ~3033–3110.
- Ticket 04 base is `074da5f` (not the feature tip) — rebase/merge on integration.
- Session-limit kills recover via SendMessage resume (agents keep context).
- Use `CONTEXT.md` terms exactly; ordinary synonyms are intentionally banned.
- Default workspace builds are GTK-free; Overlay needs `--features overlay`.
- `packaging/build-rpm.sh` requires a clean COMMITTED checkout (push not
  required) and embeds the commit.
- Whisper `prompt` honors only ~224 tokens — dictionary builder budgets it.
- Groq free-tier limits (7,200 audio-sec/hr, 2,000 req/day) are the rate ceiling.
- `rustfmt` and `clippy` are unavailable.
- Delivery still uses `clipboard_fallback` (`xkbcommon` parse errors) — 100% of
  recordings, so Raja pastes manually. Now tracked as **latency ticket 05**
  (`fix/delivery-keymap-fd`); still do NOT fold into the accuracy branch.
