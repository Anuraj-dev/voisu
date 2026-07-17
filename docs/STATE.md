# Voisu — State
> Cloud-first Linux desktop dictation app (Fedora KDE Plasma / Wayland) · Last checkpoint: 2026-07-17

## 🚧 In progress / next
- **Overlay toolkit LOCKED: keep GTK4 + gtk4-layer-shell; do NOT migrate to Electron** (2026-07-17,
  evidence-backed — see decisions.md + session log). Electron has no wlr-layer-shell on Wayland
  (`setAlwaysOnTop` no-op, positioning broken); HyprVox's Electron overlay only works via forced XWayland
  + Hyprland window rules + a rich React waveform — none of which transfer to Voisu's KWin/layer-shell/
  disposable-capsule/Rust context.
- **ACTIVE: the installed Overlay does not appear on `voisu toggle`** on Raja's live Fedora KDE/Wayland
  desktop. Diagnosing now (drive `voisu toggle`, read journal/stderr, root-cause) to produce a detailed
  fix-prompt for a separate agent. See session log 2026-07-17.
- Ticket 13 is MERGED and the live desktop smoke PASSED (`57eb284`; PR #21; issue #13 closed; CI green at
  `73f5727`). Real provider credentials set and verified (`auth verify` green for groq + deepgram).
- Then: remaining PENDING release-evidence rows (portal revocation, login start, upgrade/removal, explicit
  fallback scenarios). Final benchmark report is written (`docs/model-benchmark.md`). APT/DEB packaging is
  out of scope for this release.

## Status
- `voisu` and `voisu-daemon` communicate over bounded, versioned Unix IPC; the actor keeps status
  responsive while capture, provider completion, reconciliation, validation, and Delivery run behind
  owned boundaries.
- PipeWire capture streams one-second Deepgram chunks and bounded overlapping Groq WAV chunks
  concurrently under one Provider Deadline; cancellation owns, kills, reaps, and awaits every child.
- The Transcript pipeline selects near agreement, reconciles material disagreement, applies guardrails,
  permits one bounded repair, and otherwise falls back to a clean Source Transcript or reports a Quality
  Failure. Only the final Transcript reaches Delivery.
- Packaged-unit detection asks systemd for the EFFECTIVE unit and handles the Ticket 09 XDG stale-shadow
  case with strict conservative ExecStart parsing (see `docs/packaging-fedora.md`).
- The Fedora RPM (base GTK-free + optional overlay subpackage) is fully proven: reproducible exact-commit
  vendored build, `%check` release suite in rpmbuild, rpmlint clean, smoke-harness artifact binding, and
  the live desktop Recording→Delivery smoke (see `docs/release-evidence.md`).
- **Live-desktop fixes (2026-07-17, all RED-proven then verified on real hardware):**
  - `f876425` — `wl-copy` forks a clipboard-serving child that inherits the parent's pipes; draining
    stderr misread the healthy case as a timeout. wl-copy now runs via `run_restricted_serving`
    (discards output, trusts the parent's exit status).
  - `73f5727` — real `pw-record` catches SIGINT and exits 1 silently instead of dying by the signal, so
    every live graceful stop failed. A nonzero exit is accepted only when the child was still alive at
    the interrupt AND stderr is empty; a capture already dead before stop still fails and never delivers.
    Realistic test fakes now `exit 1` on INT (clearing the wrapper's EXIT trap so the status survives).
  - `fedora-smoke.sh` — `rpm -q` prints "package … is not installed" to stdout; the harness captured it
    as a NEVRA and tripped the clobber guard on fresh hosts. Queries now branch on rpm's exit code.
- Current gates: `cargo test --workspace` — 216 passed, 2 ignored, 0 failed (3 consecutive clean full
  runs); host rpmbuild + rpmlint + live smoke all PROVEN at `73f5727`.

## Architecture map
- Domain, IPC, lifecycle/Delivery evidence, provider coordination, decision pipeline -> `crates/voisu-core/src/lib.rs`
- Fedora adapters: PipeWire, providers, clipboard, zbus portals, native libei -> `crates/voisu-app/src/system.rs`
- systemd user-service installation, lifecycle, ownership/IPC reporting -> `crates/voisu-app/src/service.rs`
- Fedora RPM spec, exact-commit build, and smoke harness -> `packaging/`; install/upgrade/removal
  procedure -> `docs/packaging-fedora.md`
- Release evidence matrix and host checklist -> `docs/release-evidence.md`
- Headless Overlay backend selection and restart policy -> `crates/voisu-app/src/feedback.rs`
- Overlay presentation controller -> `crates/voisu-app/src/overlay.rs`
- GTK Overlay runtime adapter and observer-only status polling -> `crates/voisu-app/src/bin/voisu-overlay.rs`
- Lifecycle actor -> `crates/voisu-app/src/bin/voisu-daemon.rs`
- Public CLI -> `crates/voisu-app/src/bin/voisu.rs`
- Ordered implementation tickets -> `.scratch/voisu-implementation/issues/`

## Stack & run
- Stack: Rust 2024 + Tokio + serde + zbus 5 + GTK4 (opt-in) + runtime libei · Run:
  `cargo run -p voisu-app --bin voisu-daemon` · Test: `cargo test --workspace`

## Key decisions (top 3–5)
- Portals are the only normal Fedora path for global shortcuts and input emulation; no raw devices or `uinput`.
- Only a validated final Transcript crosses the Delivery boundary, and clipboard preservation gates
  compositor submission.
- External tools are judged by their REAL behavior, not their documented one: `wl-copy` serves via a
  pipe-holding fork (never capture its output), and `pw-record` exits 1 silently on SIGINT (accepted
  only as a live interrupt with empty stderr).
- Overlay presentation is observer-only and may disappear; the daemon lifecycle never depends on it.
- The Fedora release uses one GTK-free base RPM plus an optional Overlay subpackage; `Cargo.lock`, an
  exact-commit vendor archive, and `--offline` bind the tested source to a reproducible RPM build.
- RPM removal follows desktop-user `voisu service uninstall` before `dnf remove`, because per-user
  systemd scriptlets cannot reliably clear a running unit or enablement under `~/.config`.

## Gotchas
- Use `CONTEXT.md` terms exactly; several ordinary synonyms are banned.
- Default workspace builds are GTK-free; compile the optional Overlay with
  `cargo check -p voisu-app --features overlay`.
- This managed sandbox denies Unix-domain and private D-Bus socket binds with `EPERM`; run socket-heavy
  acceptance on the host/orchestrator.
- `rustfmt` and `clippy` are unavailable (`cargo fmt` is not installed).
- CI shared runners flake on timing-bound tests (~600ms kill bounds, service-readiness polls) under
  load; rerun before diagnosing.
- The `claude` sandbox shells have no TTY: interactive `sudo` is impossible — Raja runs sudo-needing
  commands himself in Konsole (`|& tee /tmp/…log` so the orchestrator can read the result).
