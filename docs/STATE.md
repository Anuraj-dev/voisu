# Voisu — State
> Cloud-first Linux desktop dictation app (Fedora KDE Plasma / Wayland) · Last checkpoint: 2026-07-18

## 🚧 In progress / next
- **NEXT: the TWO CRITICAL audit fixes** (decided 2026-07-18, delivery bug now
  done) — (1) supervise `process_recording`/remove `pump.await.expect`
  (voisu-daemon.rs, panic wedges daemon in Processing forever; mirror
  `supervise_replay`), (2) wrap blocking `stop_child` in `spawn_blocking`.
  Backlog charted in `.scratch/voisu-hardening/` (03 systemd-hardening +
  04 CI-audit/clippy are parallel-safe anytime; 05 hygiene waits behind
  latency).
- **Latency effort queued after the criticals** (`.scratch/voisu-latency/` +
  `docs/specs/2026-07-17-latency-optimization.md`). Note: latency ticket
  "delivery fix" item is now DONE (PR #24) — prune it from that spec when
  starting.
- Future idea (Raja, 2026-07-18, no ticket yet): packaging beyond RPM —
  Arch/pacman PKGBUILD, other distros. Code is distro-independent; purely
  packaging work. After the criticals.
- Priority 2 unchanged: Overlay visual redesign/polish (functional v1 is in).
- Deferred release acceptance still untested: logout/login startup observation,
  kill-Overlay-mid-Recording, clean uninstall.

## Status
- **DELIVERY BUG FIXED & MERGED (PR #24, merge `a570e97`, CI green).**
  Root cause: EIS keymap fd is a shared open file description with its offset
  left at EOF (compositor populates the memfd via `write()`); cursor-based
  read returned 0 bytes → xkbcommon `[XKB-822]` on empty string → every
  Delivery took `clipboard_fallback`. Fix `3b88341`: `read_keymap_fd` preads
  from absolute offset 0 (EINTR-safe, short-read-safe, trailing-NUL per
  convention) + memfd regression test pinning the EOF-offset case.
  **Live-verified on this machine: `delivery_method: compositor_submitted`**;
  clipboard fallback path untouched. 300/300 workspace tests. Sol high review:
  APPROVE, zero findings. Fix was adopted from the prior session's /tmp
  worktree (staged-uncommitted, complete) — worktree removed.
- **Accuracy branch merged earlier same day (PR #23, merge `524deda`).**
  Final live WER 10.8% raw / 9.2% formatting-adjusted (baseline 26.3%).
  Deepgram KEPT: reconciliation cuts Groq-alone 15.1%→10.9% avg.
- Local machine runs RPM `gitfd3c663` (includes both fixes).
- **Agent-entry docs changed (Raja's order): the "Delegation to Claude"
  section is DELETED** from CLAUDE.md, AGENTS.md, and the global
  ~/.claude/CLAUDE.md — never instruct codex/GPT agents to shell out to
  `claude -p` workers. Only that rule; all other routing stands.
- Full codebase audit done 2026-07-18; 2 criticals queued (see above);
  report in `.scratch/voisu-hardening/`.
- `docs/model-benchmark.md` rows 61–88 complete (committed).

## Architecture map
- Domain, IPC, Transcript decision, diagnostics -> `crates/voisu-core/src/lib.rs`
- Fedora capture/provider/clipboard/portal/libei adapters -> `crates/voisu-app/src/system.rs`
  (keymap read: `read_keymap_fd` near `keyboard_keymap_text`)
- Dictionary / Whisper prompt builder -> `crates/voisu-app/src/dictionary.rs`
- Daemon + Overlay user-service lifecycle -> `crates/voisu-app/src/service.rs`
- Public CLI -> `crates/voisu-app/src/bin/voisu.rs`
- GTK Overlay -> `crates/voisu-app/src/bin/voisu-overlay.rs`, `overlay.rs`, `feedback.rs`
- RPM units/spec/build/smoke -> `packaging/`
- Deepgram real-time diagnostic probe -> `crates/voisu-app/examples/deepgram_probe.rs`
- Accuracy effort -> `docs/specs/2026-07-17-transcription-accuracy.md`, `.scratch/voisu-accuracy/`
- Latency effort -> `docs/specs/2026-07-17-latency-optimization.md`, `.scratch/voisu-latency/`
- Fedora procedure/evidence -> `docs/packaging-fedora.md`, `docs/release-evidence.md`

## Stack & run
- Stack: Rust 2024 + Tokio + serde + zbus 5 + GTK4 (opt-in) + runtime libei · Run: `cargo run -p voisu-app --bin voisu-daemon` · Test: `cargo test --workspace`
- Robust test count: `cargo test --workspace 2>&1 | grep -oE "[0-9]+ passed; [0-9]+ failed" | awk '{p+=$1; f+=$3} END {print p, f}'`
- Test baseline: **300** (added keymap regression test in PR #24).

## Key decisions (top 3–5)
- EIS/portal keymap fds must be read with `pread` at offset 0, never through
  the shared file cursor — the offset is compositor-controlled state.
- Never arm PR_SET_PDEATHSIG from a transient thread: pdeathsig fires on the
  forking THREAD's exit — external children must be spawned from a thread that
  outlives them (capture reader thread), never the Tokio blocking pool.
- rustls crypto backend = ring (explicit install at startup); aws-lc-rs
  rejected (cmake dep breaks vendored RPM builds).
- Transcription: Groq single-request + vocabulary prompt primary; Deepgram
  nova-3 streaming second opinion, disableable.
- Portals are the normal Fedora path; no raw input devices or `uinput`.

## Gotchas
- **Disk critically tight (~14 GB).** `cargo clean` before RPM builds; build
  needs `TMPDIR=/var/tmp RUST_TEST_THREADS=4`; script refuses a dirty tree
  (untracked count; `.git/info/exclude` locally ignores `.claude/`,
  `.scratch/voisu-latency/`, latency spec).
- The `fix/delivery-keymap-fd` worktree under /tmp is GONE (merged, removed) —
  no parallel branches outstanding; `system.rs` keymap region is free to touch.
- `packaging/build-rpm.sh` requires a clean COMMITTED checkout; embeds commit.
- Whisper `prompt` honors only ~224 tokens; Groq free tier: 7,200 audio-sec/hr,
  2,000 req/day.
- Use `CONTEXT.md` terms exactly; default builds GTK-free (`--features overlay`
  for Overlay). `rustfmt`/`clippy` unavailable.
- Leftover diagnostics (optional cleanup): `/var/tmp/pwtest.raw`,
  `/var/tmp/pwpipe.err`, fixture `pwtest.raw` under
  `/run/user/1000/voisu/v1/diagnostics/fixtures/`.
