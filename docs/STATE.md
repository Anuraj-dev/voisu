# Voisu — State
> Cloud-first Linux desktop dictation app (Fedora KDE Plasma / Wayland) · Last checkpoint: 2026-07-17

## 🚧 In progress / next
- **Execute the approved transcription-accuracy PRD** —
  `docs/specs/2026-07-17-transcription-accuracy.md` (READY FOR APPROVAL;
  Raja approved verbally in-session 2026-07-17, execution greenlit). Map:
  `.scratch/voisu-accuracy/map.md` (tickets 01–02 closed, 03 PRD written,
  04–07 open). Next concrete step: create `feature/transcription-accuracy`
  branch, then spawn 3 parallel implementers — ticket 04 Groq accuracy
  (Opus), ticket 05 Deepgram nova-3 websocket (Fable 5 medium —
  architectural), ticket 06 reconciliation gating + provider-failure
  visibility (Opus). Driver is purely orchestrator; Sol reviews (first high,
  re-reviews medium). Local RPM install + live 4-paragraph suite BEFORE any
  PR; PR + merge on CI green only if accuracy AND latency win.
- **Accuracy diagnosis is DONE** (2026-07-17 blind test, 26.3% WER):
  root causes = Deepgram 1 s batch-chunk design (word salad / silent
  absence), Groq without prompt/language (jargon errors), 30 s chunk seams.
  Reconciliation was NOT the villain — the old STATE hypothesis is refuted;
  it slightly improved Groq in the one reconciled recording.
- Priority 2 unchanged: Overlay visual redesign/polish (functional v1 is in).
- Deferred release acceptance still untested: logout/login startup
  observation, kill-Overlay-mid-Recording, clean uninstall.

## Status
- Overlay graphical startup merged locally (`67aa3b6`); local `main` ahead of
  `origin/main`, not pushed. Packaged units installed and verified live.
- Accuracy PRD written and bound to research: Deepgram streaming guide (in
  Raja's vault: `AI Created Stuff/Hyprvox Rebuild/Deepgram Streaming Guide
  for Voisu.md`) + pricing asset
  (`.scratch/voisu-accuracy/assets/02-groq-deepgram-pricing.md`).
- Key PRD decisions: Groq full-audio-at-finalize ≤120 s (evidence: tail
  request already ~400–500 ms, so no latency cost) + `whisper-large-v3`
  default (Groq free tier covers 2 h/day for both models — accuracy decides);
  vocabulary prompt = built-in dev dictionary + `~/.config/voisu/dictionary.txt`
  (224-token budget, user terms first), same list feeds Deepgram `keyterm`;
  Deepgram rebuilt as nova-3 websocket streaming (tokio-tungstenite rustls,
  is_final-only assembly, visible failure, disableable); reconciliation on
  `llama-3.1-8b-instant`; gate reconciliation on source comparability.
- Deepgram stays past the $200 credit (Raja rotates accounts); future
  (post-acceptance, not now): single-provider benchmark.
- Automated gates green as of last session: 221 passed, 2 live ignored, 0
  failed.

## Architecture map
- Domain, IPC, Transcript decision, diagnostics -> `crates/voisu-core/src/lib.rs`
- Fedora capture/provider/clipboard/portal/libei adapters -> `crates/voisu-app/src/system.rs`
  (GroqStream ~1534, request_groq_chunk ~3574, DeepgramStream ~1799,
  merge_chunk_transcripts ~3640, Groq reconciliation ~1966–2100)
- Daemon + Overlay user-service lifecycle -> `crates/voisu-app/src/service.rs`
- Public CLI -> `crates/voisu-app/src/bin/voisu.rs`
- GTK Overlay -> `crates/voisu-app/src/bin/voisu-overlay.rs`, `overlay.rs`, `feedback.rs`
- RPM units/spec/build/smoke -> `packaging/`
- Accuracy effort -> `docs/specs/2026-07-17-transcription-accuracy.md` (PRD),
  `.scratch/voisu-accuracy/` (map + tickets + pricing asset)
- Fedora procedure/evidence -> `docs/packaging-fedora.md`, `docs/release-evidence.md`

## Stack & run
- Stack: Rust 2024 + Tokio + serde + zbus 5 + GTK4 (opt-in) + runtime libei · Run: `cargo run -p voisu-app --bin voisu-daemon` · Test: `cargo test --workspace`

## Key decisions (top 3–5)
- Transcription accuracy overhaul per PRD: Groq single-request + vocabulary
  prompt is the primary accuracy lever; Deepgram = real streaming second
  opinion, cleanly disableable (only non-free component).
- Acceptance bar: ≤10% WER on the live 4-paragraph technical suite, no
  fluent-nonsense substitutions, no silent provider absence, latency ≤ today.
- Overlay presentation stays observer-only and disposable; daemon never
  depends on it.
- Keep GTK4 + gtk4-layer-shell for the KWin layer-shell surface.
- Portals are the normal Fedora path; no raw input devices or `uinput`.

## Gotchas
- Use `CONTEXT.md` terms exactly; ordinary synonyms are intentionally banned.
- Default workspace builds are GTK-free; Overlay needs `--features overlay`.
- `packaging/build-rpm.sh` requires a clean COMMITTED checkout (push not
  required — fine for feature-branch local testing) and embeds the commit.
- Whisper `prompt` honors only ~224 tokens — dictionary builder must budget.
- Deepgram history absence was silent by design flaw; until ticket 06 lands,
  a missing source Transcript says nothing about why.
- Groq free-tier limits (7,200 audio-sec/hr, 2,000 req/day) are the real
  rate ceiling — full-audio single requests help stay under the req/day cap.
- `rustfmt` and `clippy` are unavailable.
- Delivery still uses `clipboard_fallback` (`xkbcommon` parse errors) —
  separate effort, do not fold into the accuracy branch.
