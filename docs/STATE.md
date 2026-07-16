# Voisu — State
> Cloud-first Linux desktop dictation app (Fedora KDE Plasma / Wayland) · Last checkpoint: 2026-07-16

## 🚧 In progress / next
- Ticket 13 on `ticket-13-fedora-package`. Sol round-2 (4) and round-3 (3 HIGH + 2 MEDIUM) findings are all FIXED.
  Packaged-unit detection resolves the EFFECTIVE unit via `systemctl --user show -p LoadState -p FragmentPath -p
  ExecStart` and, crucially, reaches the stale-shadow migration case: a Ticket 09 XDG unit outranks `/etc` and
  `/usr/lib` in systemd precedence, so when the effective fragment is the XDG unit (or systemctl can't answer) the
  packaged unit is detected on disk and migrated. Validation now rejects any non-`loaded` LoadState (with a fallback
  reason) and validates EVERY ExecStart command binary, not just the first. The fake `systemctl show` models real
  precedence (XDG shadows packaged) so the migration test is honest; RED-proven. The smoke binds to the supplied RPM
  via full `rpm --dump` manifest comparison, and its user-service restoration is now verified (each failure printed,
  forces non-zero exit even on an otherwise-passing smoke; enabled-runtime handled; unrestorable states reported).
  The vendor `Source1` self-test now runs an INDEPENDENT `cargo vendor` of the same commit and compares byte-for-byte
  (proving cargo-vendor stability, not just tar determinism), with `--mode` normalization against umask drift. Both
  packaging scripts capture `rpm`/`tar` output before grepping to avoid SIGPIPE-141 under pipefail.
- Sol round-4 findings (1 HIGH + 2 MEDIUM) are fixed by the driver: the on-disk unit-file ExecStart parser is strict
  and conservative — absolute unquoted executables with attached `@-:+!` prefixes and empty-assignment reset only;
  quoting, line continuations, and prefixes separated from their executable are refused with an explicit reason
  instead of guessed at (install then stays on the Ticket 09 path). The `systemctl show` parser only matches
  block-opening `{ path=`, so argv arguments like `--config-path=/tmp` are never validated as command binaries. The
  smoke's restoration is judged on END STATE: post-restore enablement/active is compared to the snapshot, and the
  fresh-install cleanup verifies the RPM is gone and the unit not left enabled — any mismatch forces a non-zero exit.
  Four new tests, all RED-proven against the round-3 parsers. Next: Sol MEDIUM round-5 re-review.
- Host RPM build gate PASSED (rpm-build/rpmlint/systemd-rpm-macros installed): `packaging/build-rpm.sh` at `674b93e`
  produced base + overlay + debuginfo RPMs and the SRPM in `dist/rpm/` with the offline vendored build and the full
  `%check` release suite green; the first host run caught the real SIGPIPE defect fixed in `674b93e`. `rpmlint`: 2
  minor errors (explicit-lib-dependency libsecret → polish to `/usr/bin/secret-tool`; cosmetic changelog version) and
  6 cosmetic warnings.
- After APPROVE: rpmlint spec polish, refresh `docs/release-evidence.md` (rpmbuild/%check rows PROVEN; live rows
  pending), add Ticket 13 rows to `docs/model-benchmark.md` (Luna high impl `a6b7934` ~296k tokens; Luna xhigh fix
  `a4e978e` ~301k tokens; Opus escalations ~143k + ~232k; Sol reviews; driver round-4 fixes), rebuild the RPM at the
  final HEAD, open the PR, exact-head CI, squash-merge, close issue #13.
- Still needs Raja/desktop: the live `packaging/fedora-smoke.sh` run (`VOISU_FEDORA_LIVE_SMOKE=1`: real install,
  service start, Recording, Delivery via `wl-paste`); ticket live checkboxes stay unchecked until then.
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
  detection asks systemd for the EFFECTIVE unit (`systemctl --user show -p LoadState -p FragmentPath -p ExecStart`)
  so administrator `/etc` overrides and drop-ins are honored, and validates every effective ExecStart command binary.
  Because a Ticket 09 XDG user unit outranks the packaged dirs in systemd precedence, when the effective fragment is
  that XDG unit (or systemctl cannot answer) the packaged unit is detected on disk and migrated — the real
  stale-shadow case. A non-`loaded` LoadState or any missing/untrusted ExecStart binary falls back to the Ticket 09
  user-data path with an explicit reason; a user unit with no packaged file on disk is never treated as packaged;
  daemon lifecycle remains independent from the optional Overlay.
- Ticket 12 keeps the Overlay observer-only: a pure, headless selection layer chooses runtime-advertised Layer Shell,
  an unfocusable regular GTK surface, desktop notification, or a persistent journal observer when no display exists.
  Structured Overlay logs and `voisu-overlay --report-backend` expose `backend` plus `degradation`; normal `voisu
  status` remains daemon-only. A missing dynamic GTK runtime fails before `main` and is recorded by the launching
  service/journal rather than falsely selected as an Overlay backend. `voisu-overlay --supervise` bounds separate
  Overlay restarts to three failures in 30 seconds and never touches the daemon.
- Ticket 12 round-2 review fixes are in: the Overlay's surface probe is now honest local GTK realization (no unsound
  compositor-map timer, no false fallback on a healthy compositor), and the capsule stays hidden at Idle with no startup
  flash while status polling starts immediately.
- Current gates: `cargo test --workspace` — 211 passed, 2 ignored, 0 failed (effective-unit + shadow-migration +
  LoadState + multi-ExecStart tests; shadow-migration RED-proven); `cargo check -p voisu-app --features overlay`,
  `cargo build --workspace`, `bash -n` for packaging scripts, and `git diff --check` are clean. Deterministic vendor
  archiving verified byte-identical across umasks headlessly; `rpmbuild` and RPM/live Fedora evidence are pending the
  host (build-rpm.sh had its first real host run; SIGPIPE fix at 674b93e).

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
