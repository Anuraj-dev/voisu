# Voisu — State
> Cloud-first Linux desktop dictation app (Fedora KDE Plasma / Wayland) · Last checkpoint: 2026-07-16

## 🚧 In progress / next
- Ticket 13 on `ticket-13-fedora-package`. Sol round-2 review found 3 HIGH + 1 MEDIUM; all four are now FIXED on the
  branch: (1) packaged-unit detection resolves the EFFECTIVE unit via `systemctl --user show -p FragmentPath -p
  ExecStart` (honors `/etc` overrides + drop-ins, validates the effective ExecStart binary, XDG-config units are not
  packaged, static `/etc`-before-`/usr/lib` fallback only when systemctl is unavailable) with three discriminating
  tests; (2) the smoke binds to the supplied RPM by comparing full `rpm -qp --dump` vs `rpm -q --dump` manifests and
  refuses a same-NEVRA payload mismatch; (3) the vendor `Source1` is reproducible — vendored from the exact-commit
  git-archive extraction and archived with deterministic tar (`--sort`/`--owner`/`--group`/`--numeric-owner`/`--mtime`
  + `gzip -n`), with a byte-identical re-archive self-test; (4) the smoke snapshots and restores the user-service
  state it mutates in a cleanup trap that runs on failure too. Next: Sol MEDIUM round-3 re-review.
- After APPROVE: add Ticket 13 rows to `docs/model-benchmark.md` (Luna high impl `a6b7934` ~296k tokens; Luna xhigh
  fix `a4e978e` ~301k tokens; Opus escalation; Sol high + medium reviews), open the PR, exact-head CI, squash-merge,
  close issue #13.
- Blocked on Raja: `sudo dnf install -y rpm-build rpmlint systemd-rpm-macros`, then `packaging/build-rpm.sh` on the
  host plus the live `packaging/fedora-smoke.sh` checks in `docs/release-evidence.md`; ticket host checkboxes stay
  unchecked until then.
- After Ticket 13 merges: write the final model-benchmark report (Sol/Terra/Luna vs Opus, incl. Luna
  medium/high/xhigh effort comparison) that Raja requested.

## Status
- `voisu` and `voisu-daemon` communicate over bounded, versioned Unix IPC; the actor keeps status responsive while
  Recording capture, provider completion, reconciliation, validation, and Delivery run behind owned boundaries.
- PipeWire capture streams one-second Deepgram chunks and bounded overlapping Groq WAV chunks concurrently under one
  Provider Deadline. Cancellation owns, kills, reaps, and awaits every child before Idle.
- The Transcript pipeline deterministically selects near agreement, reconciles material disagreement, applies
  Unicode-aware guardrails, permits one bounded repair, and otherwise falls back to a clean Source Transcript or
  reports a Quality Failure. Only the final Transcript reaches Delivery.
- Ticket 09 installs a graphical-session-owned daemon service with atomic binaries and a three-starts-per-30-seconds
  failure bound; the packaged unit is preferred over and migrates away old XDG user-data ownership. Packaged-unit
  detection asks systemd for the EFFECTIVE unit (`systemctl --user show -p FragmentPath -p ExecStart`) so
  administrator `/etc` overrides and drop-ins are honored, validates the effective ExecStart binary, and only falls
  back to a static `/etc`-before-`/usr/lib` search when systemctl is unavailable; a user unit under XDG config is
  never treated as packaged; invalid packaged metadata clearly falls back to the Ticket 09 user-data path; daemon
  lifecycle remains independent from the optional Overlay.
- Ticket 12 keeps the Overlay observer-only: a pure, headless selection layer chooses runtime-advertised Layer Shell,
  an unfocusable regular GTK surface, desktop notification, or a persistent journal observer when no display exists.
  Structured Overlay logs and `voisu-overlay --report-backend` expose `backend` plus `degradation`; normal `voisu
  status` remains daemon-only. A missing dynamic GTK runtime fails before `main` and is recorded by the launching
  service/journal rather than falsely selected as an Overlay backend. `voisu-overlay --supervise` bounds separate
  Overlay restarts to three failures in 30 seconds and never touches the daemon.
- Ticket 12 round-2 review fixes are in: the Overlay's surface probe is now honest local GTK realization (no unsound
  compositor-map timer, no false fallback on a healthy compositor), and the capsule stays hidden at Idle with no startup
  flash while status polling starts immediately.
- Current gates: `cargo test --workspace` — 205 passed, 2 ignored, 0 failed (3 new effective-unit tests);
  `cargo check -p voisu-app --features overlay`, `cargo build --workspace`, and `bash -n` for packaging scripts are
  clean. Deterministic vendor-archive tar/gzip verified byte-identical headlessly; `rpmbuild` and RPM/live Fedora
  evidence are pending the host.

## Architecture map
- Domain, IPC, lifecycle/Delivery evidence, provider coordination, decision pipeline -> `crates/voisu-core/src/lib.rs`
- Fedora adapters: PipeWire, providers, clipboard, zbus portals, native libei -> `crates/voisu-app/src/system.rs`
- systemd user-service installation, lifecycle, ownership/IPC reporting -> `crates/voisu-app/src/service.rs`
- Fedora RPM spec, exact-commit build, and smoke harness -> `packaging/`; install/upgrade/removal procedure ->
  `docs/packaging-fedora.md`
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
- Only a validated final Transcript crosses the Delivery boundary, and clipboard preservation gates compositor submission.
- The daemon service is graphical-session owned and rate-limits only persistent startup failure; Recording failures
  never replay Delivery.
- Overlay presentation is observer-only and may disappear; Layer Shell is a runtime compositor capability, with
  separate regular-surface/notification feedback and a bounded Overlay-only supervisor.
- Every spawned external process receives a guarded Linux parent-death signal.
- The Fedora release uses one GTK-free base RPM plus an optional Overlay subpackage; `Cargo.lock`, an exact-commit
  vendor archive, and `--offline` bind the tested source to a reproducible RPM build.
- RPM removal follows desktop-user `voisu service uninstall` before `dnf remove`, because per-user systemd
  scriptlets cannot reliably clear a running unit or enablement under `~/.config`.

## Gotchas
- Use `CONTEXT.md` terms exactly; several ordinary synonyms are banned.
- Default workspace builds are GTK-free; compile the optional Overlay with
  `cargo check -p voisu-app --features overlay`.
- This managed sandbox denies Unix-domain and private D-Bus socket binds with `EPERM`; run socket-heavy acceptance on
  the host/orchestrator.
- `rustfmt` and `clippy` are unavailable (`cargo fmt` is not installed).
