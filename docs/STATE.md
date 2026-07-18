# Voisu — State
> Cloud-first Linux desktop dictation app (Fedora KDE Plasma / Wayland) · Last checkpoint: 2026-07-19 (night)

## 🚧 In progress / next
- **Friends map (.scratch/voisu-friends/, GH #32) — phase A is 7/8 done.** Remaining:
  1. **Ticket 06 (keyring probe, GH #38): BLOCKED on Raja rebooting/logging in fresh.** Dispatch after.
  2. **Ticket 07 (setup wizard, GH #39):** after 06.
  3. **Phase B (tickets 09–15, GH #41–47):** 09 is Raja's guided accounts HITL checklist, then packaging.
- **HITL queue for Raja:** (a) live KDE test of guarded mode (`voisu delivery guarded` → restart → focus-hold
  auto-types / focus-switch falls to clipboard+notification) — merge NOT gated, post-merge check;
  (b) reboot for ticket 06; (c) live GNOME VM visual check of the overlay fallback before friend rollout;
  (d) on next RPM install: DELETE `~/.config/systemd/user/voisu{,-overlay}.service.d/sandbox-validation.conf`.
- **ROUTING CHANGE (Raja, 2026-07-19, pinned in CLAUDE.md):** Codex quota nearly exhausted — Sol/Codex =
  REVIEWS ONLY; ALL implementation → Opus 4.8 subagents (Fable subagent if architectural). No Terra/Luna.

## Status
- **Fix batch DONE (2026-07-18/19):** Deepgram default ON (PR #48), hardening-05 sweep (PR #49),
  keyterm cap by priority (PR #51), service-ready deadline split (PR #52), overlay unavailable-capsule
  fix (PR #54). The externally-reported "Daemon unavailable never times out" finding is FULLY RESOLVED
  (#54 edge-triggered flash + separate unavailable_until deadline; #59 resurface/notify tracker).
- **Map tickets closed:** 01 ADR 0007 (PR #53) · 02 delivery_mode (PR #55) · 03 focus research (asset)
  · 04 guarded delivery (PR #56) · 05 dictionary CLI + hot-reload (PR #57) · 08 GNOME fallback (PR #59).
- Test baseline: **391 passed / 0 failed**, both default and `--features voisu-app/overlay`.
- CI flake tracked: **GH #58** — managed_service_lifecycle goes *inactive* under the 3x parallel flake
  gate (4 hits, always passes on rerun; deadline bump didn't fix; needs journal capture or isolation).
- Repo is now **PUBLIC** (Raja: free Actions minutes; private billing was blocked).
- `docs/model-benchmark.md` rows through 156 (Opus/Sol per-ticket verdicts this session).

## Architecture map
- Domain, IPC, Transcript decision, FocusProbe trait -> `crates/voisu-core/src/lib.rs`, `diagnostics.rs`
- Fedora capture/provider/clipboard/portal/libei adapters + GuardedDelivery + GroqProvider::with_prompt -> `crates/voisu-app/src/system.rs`
- Focus probes (KWin script/D-Bus push + sender auth, hyprctl, Null) -> `crates/voisu-app/src/focus.rs`
- Recording/replay supervision, per-Recording dictionary snapshot, delivery_mode wiring -> `crates/voisu-app/src/bin/voisu-daemon.rs`
- Persisted config (deepgram_enabled + delivery_mode, both-key writer) -> `crates/voisu-app/src/config.rs`
- Dictionary file edits (flock-serialized) + Whisper prompt + keyterm cap -> `crates/voisu-app/src/dictionary.rs`
- Public CLI (`voisu deepgram|delivery|dictionary|history`; setup lands here) -> `crates/voisu-app/src/bin/voisu.rs`
- GTK Overlay + pure controllers (PresentationController/Tracker, NotifyLatch, poll_tick) -> `crates/voisu-app/src/bin/voisu-overlay.rs`, `overlay.rs`, `feedback.rs`
- Daemon + Overlay user-service lifecycle -> `crates/voisu-app/src/service.rs`
- RPM units/spec/build/smoke -> `packaging/` · CI gates -> `.github/workflows/ci.yml`
- Friends map -> `.scratch/voisu-friends/` (map.md has per-ticket resolution lines; issues/01–15)
- Research digest -> `.scratch/voisu-research/2026-07-18-distribution-decisions.md`

## Stack & run
- Stack: Rust 2024 + Tokio + serde + zbus 5 + GTK4 (opt-in) + runtime libei + flacenc · Run: `cargo run -p voisu-app --bin voisu-daemon` · Test: `cargo test --workspace`
- Robust test count: `cargo test --workspace 2>&1 | grep -oE "[0-9]+ passed; [0-9]+ failed" | awk '{p+=$1; f+=$3} END {print p, f}'`

## Key decisions (top; full log in decisions.md)
- **Codex = reviews only; Claude implements everything** (Raja 2026-07-19; quota). Sol first review high, re-reviews medium; Sol-implemented code → Opus reviews.
- **Guarded delivery**: strict stable_id-only match; fail closed on unknown; KWin all-string D-Bus wire (callDBus INT32 trap) + sender auth + 10-min staleness bound; long-dwell clipboard fallback is the accepted tradeoff.
- **Dictionary edits flock-serialized**; per-Recording snapshot feeds BOTH providers; supervised tail never touches the fs.
- **GNOME**: plain-window fallback with re-present-on-visible-transition + Recording notification from OBSERVED daemon states; wl-copy shell-out stays (Flatpak-proofing = phase B).
- **GTK4 locked, Electron rejected** (ADR 0007); packaging = cargo-deb + AUR + COPR + apt repo, one on-tag workflow.

## Gotchas
- **cladex JSON output carries ONLY the final message** — every review prompt must demand self-contained
  final findings (one Sol review was lost and re-dispatched; rule now standing).
- **No local clippy (no rustup)** — CI is the only clippy oracle; keep diffs lint-clean by inspection.
- Disk tight (~7 GB free after cleanup; target/debug/incremental deleted 2026-07-19, wavs purged).
  `cargo clean` before RPM builds; `TMPDIR=/var/tmp RUST_TEST_THREADS=4`.
- **Don't switch branches while a tree-using agent runs** (one collision + one near-miss this session).
- CI flake #58: rerun the test gate once before investigating; if it recurs on the same PR twice, stop.
- Deepgram keyterms capped (user-first, 500-token/100-term) — safe now, still don't bloat the glossary.
- `packaging/build-rpm.sh` needs a clean COMMITTED checkout · COPR builders have NO network (vendor crates).
- Hyprland RemoteDesktop/EIS needs a live smoke before promising auto-type on Omarchy.
- Use `CONTEXT.md` terms exactly; default builds GTK-free (`--features overlay` for the Overlay).
