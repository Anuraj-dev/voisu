# Voisu — State
> Cloud-first Linux desktop dictation app (Fedora KDE Plasma / Wayland) · Last checkpoint: 2026-07-18 (evening)

## 🚧 In progress / next
- **NEW EFFORT CHARTED: "Voisu to friends" (three distros).** Wayfinder map at
  `.scratch/voisu-friends/map.md`, mirrored as GitHub issue #32 (tickets
  #33–#47; local files canonical). Handoff prompt for the next orchestrator
  session ready at `.scratch/voisu-friends/handoff-prompt.md`.
- Next session works, in order:
  1. **Fix batch (outside the map):** Deepgram default OFF→ON flip;
     hardening-05 sweep (`.scratch/voisu-hardening/issues/05` + BoundaryError
     boxing + wait_for_marker 3 s→15 s); **keyterm cap fix** (NEW BUG: uncapped
     keyterms → Deepgram 400 past 500 tokens kills the stream — digest §6).
  2. **Map phase A (features):** ADR GTK4/Electron, delivery_mode enum incl.
     guarded (focus-guard, in scope now), dictionary CLI + hot-reload, keyring
     probe → setup wizard, GNOME plain-window fallback.
  3. **Map phase B (packaging):** accounts HITL → cargo-deb, AUR, COPR, apt
     repo, on-tag release CI + container smoke, live desktop validation.
- On the **next RPM build/install**: DELETE the validation drop-ins at
  `~/.config/systemd/user/voisu{,-overlay}.service.d/sandbox-validation.conf`.

## Status
- **2026-07-18 evening: 13-scout research fleet + grilling session** produced
  all distribution/roadmap decisions (benchmark rows 122–134; digest with
  decisions at `.scratch/voisu-research/2026-07-18-distribution-decisions.md`).
  Fact-checked adversarially: 6/8 claims confirmed, 2 softened, 0 wrong.
- **Latency effort CLOSED (all 5 tickets).** Groq-only tails 474–1075 ms vs
  reconciled 727–1670 ms; reconciliation repaired 8/9 jargon probes Groq-only
  mangled. Deepgram KEPT; default flips ON in the fix batch.
- **Hardening 01–04 CLOSED** (03 validated+merged PR #31; 04 CI gates PR #29).
  CI: tests + clippy -D warnings + cargo-audit on every PR.
- Installed RPM: git58a607f. Test baseline: **346** (330 + 16 history/CLI).
- `docs/model-benchmark.md` rows 103–134 (Sol/Opus verdict + research fleet).

## Architecture map
- Domain, IPC, Transcript decision, diagnostics -> `crates/voisu-core/src/lib.rs`, `diagnostics.rs`
- Fedora capture/provider/clipboard/portal/libei adapters + ProviderReaper + FLAC + Deepgram keyterm URL -> `crates/voisu-app/src/system.rs`
- Recording/replay supervision + DisabledProvider -> `crates/voisu-app/src/bin/voisu-daemon.rs`
- Persisted config (deepgram toggle; delivery_mode lands here) -> `crates/voisu-app/src/config.rs`
- History pretty-rendering -> `crates/voisu-app/src/history_view.rs`
- Dictionary / Whisper prompt builder -> `crates/voisu-app/src/dictionary.rs`
- Daemon + Overlay user-service lifecycle -> `crates/voisu-app/src/service.rs`
- Public CLI (`voisu deepgram|history`; dictionary/delivery/setup land here) -> `crates/voisu-app/src/bin/voisu.rs`
- GTK Overlay -> `crates/voisu-app/src/bin/voisu-overlay.rs`, `overlay.rs`, `feedback.rs`
- RPM units/spec/build/smoke -> `packaging/` · CI gates -> `.github/workflows/ci.yml`
- Friends-distribution map -> `.scratch/voisu-friends/` (map.md, issues/01–15, handoff-prompt.md; GH #32–47)
- Research digest (all 2026-07-18 decisions + evidence) -> `.scratch/voisu-research/2026-07-18-distribution-decisions.md`
- Hardening map -> `.scratch/voisu-hardening/` (05 open, queued in fix batch)

## Stack & run
- Stack: Rust 2024 + Tokio + serde + zbus 5 + GTK4 (opt-in) + runtime libei + flacenc · Run: `cargo run -p voisu-app --bin voisu-daemon` · Test: `cargo test --workspace`
- Robust test count: `cargo test --workspace 2>&1 | grep -oE "[0-9]+ passed; [0-9]+ failed" | awk '{p+=$1; f+=$3} END {print p, f}'`

## Key decisions (top; full log in decisions.md)
- **GTK4 locked, Electron rejected** (no layer-shell path in Chromium; ADR = map ticket 01; Tauri only web-tech fallback).
- **delivery_mode enum type|clipboard|guarded — guarded IN scope** (focus-guard; no competitor ships it).
- **STT stays two-mode** (reconciled default + Groq-only); no Deepgram-only third mode.
- **Packaging: cargo-deb + AUR(-bin) + COPR + self-hosted apt repo, one on-tag workflow; Flatpak later, AppImage never; GNOME ships plain-window fallback (Shell extension = later polish).**
- **Pure BYOK + `voisu setup` wizard + keyring** (free tiers cover friends: Groq 8 h/day, Deepgram $200 ≈ 1 yr).
- Sequencing: fixes → features → packaging; **2 failed review rounds → discard agent, Fable inline or higher effort**.

## Gotchas
- **Disk critically tight (~11–14 GB).** `cargo clean` before RPM builds; `TMPDIR=/var/tmp RUST_TEST_THREADS=4`.
- **No local clippy/rustfmt (no rustup)** — CI is the only clippy run; errors surface serially.
- **Pin cwd (`git -C …`) in compound git commands when a worktree is active.**
- cladex: prompt via stdin with `-p`+flags; `--permission-mode acceptEdits` + explicit `--allowedTools`.
- **Subagent doc fence** in every dispatch prompt (docs/STATE, sessions, benchmark) — held 13/13 this session.
- The accuracy WER suite assumes Deepgram ON (default will match after the flip).
- `packaging/build-rpm.sh` needs a clean COMMITTED checkout; embeds commit.
- Whisper prompt ~224 tokens (last-224 honored); Groq free tier: 7,200 audio-sec/hr, 2,000 req/day.
- **Deepgram keyterms: 500-token/100-term hard cap, 400 error past it — cap fix queued, don't grow the built-in glossary before it lands.**
- COPR builders have NO network (vendor crates); Hyprland RemoteDesktop/EIS needs a live smoke test before promising auto-type on Omarchy.
- Use `CONTEXT.md` terms exactly; default builds GTK-free (`--features overlay`).
