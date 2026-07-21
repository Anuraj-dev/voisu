# Voisu — State
> Cloud-first Linux desktop dictation app (Fedora KDE Plasma / Wayland) · Last checkpoint: 2026-07-21 (~16:35)

## 🚧 In progress / next
- **v0.1.0 IS RELEASED on ALL FOUR CHANNELS** (this session): COPR `0.1.0-1` (Raja installed from it),
  apt repo live on gh-pages (signed, Valid-Until +30d), GitHub Release v0.1.0 (deb+tarball+SHA256SUMS),
  AUR `voisu` + `voisu-bin` (first push done LOCALLY via podman arch container — CI job had the tarball
  blob bug, fixed in PR #75). Landing page live: https://anuraj-dev.github.io/voisu/ (gh-pages).
- **NEXT RELEASE STEP: tag v0.1.1 (Raja, per RELEASING.md)** — main @80e6ecc now carries BOTH of today's
  fixes (PR #76 + PR #77); nothing reaches installed users until the tag (COPR make-srpm.sh deliberately
  builds the highest v-tag, NOT HEAD — so today's COPR rebuild was a no-op v0.1.0 rebuild by design).
  After v0.1.1: Raja's §8 verify (one final Trigger Key prompt, then silence; prune 31 leaked sections
  per packaging-fedora.md), Arch friend upgrades (fresh-home unit fix + Hyprland docs).
- **MERGED: PR #77 (first-run gaps, from live Arch/Hyprland field failures):** unit now provisions
  config/state dirs via ConfigurationDirectory/StateDirectory (fresh home 226/NAMESPACE fixed by
  construction), smoke guards ×5, doctor GlobalShortcuts probe + failed-unit→journalctl hint, README
  per-desktop Trigger Key (Hyprland bind read from `hyprctl globalshortcuts` — app-id half is
  environment-dependent, never hardcode), .github issue template. 448/0.
- **NEW PARKED QUESTION (c) for Raja: Hyprland Type Delivery** — xdg-desktop-portal-hyprland implements
  NO RemoteDesktop interface (only Screenshot/ScreenCast/GlobalShortcuts), so typing can't work there;
  README now says `voisu delivery clipboard`. Decide: clipboard-only Hyprland vs a wlr virtual-keyboard
  delivery backend (new feature, spec first). Doctor RemoteDesktop probe = easy follow-up either way.
- **MERGED: PR #76 (main @64ae3de) — Global Shortcuts re-prompt fix.** Root cause: PID-baked
  `session_handle_token` + no .desktop → portal-kde made a NEW kglobalaccel component per daemon start →
  Trigger Key dialog on every restart + one leaked `[token_voisu_session_<pid>]` section per start (31 on
  Raja's machine). Fix: constant `"voisu_session"` token (request tokens stay PID-unique), regression test,
  `packaging/voisu.desktop` shipped by RPM/deb/AUR×2/release-tarball, manual cleanup doc in
  packaging-fedora.md. NOT in the installed 0.1.0-1 RPM — **next: rebuild (COPR re-poke does it) +
  reinstall + verify §8 repro (restart ×2, no dialog, section count frozen); expect ONE final prompt
  post-upgrade; Raja then hand-prunes the 31 leaked sections + stray `[Alacritty]` voisu-toggle line.**
- **IN FLIGHT: service_cli deflake** — resumed flake-agent working on
  `managed_service_lifecycle_reports_systemd_ownership_and_daemon_ipc` ("service did not become ready"
  in COPR f43 %check, build 10757368) + a sweep of service_cli.rs for the tight-deadline family.
  Test-only, no retag needed. When it reports: driver verifies → PR → Opus review if substantial → merge.
- **Then:** re-poke the failed COPR rebuild (cosmetic — published packages unaffected):
  `copr-cli build-package voisu --name voisu`.
- **NEXT TICKET: 15 (live desktop validation, `.scratch/voisu-friends/issues/15-*.md`)** — HITL-heavy;
  Raja informally validated install+run today (COPR install works, overlay works, "everything worked");
  ticket 15 makes it systematic. Also: scripted apt smoke test (packaging/apt/README.md) never run.
- **Two questions still parked for Raja:** (a) model benchmark FINAL REPORT now vs more data (rows→195);
  (b) quality-gate transcript drops product decision — ASK before speccing.
- **Remaining HITL (old queue, still open):** reboot+suspend Trigger-Key self-heal check (PR #61); live
  KDE guarded-mode test (PR #56); GNOME VM visual check (PR #59); `voisu setup` vs real ksecretd
  (PR #62/#70); AUR TOTP when aurweb ships 2FA; delete sandbox-validation.conf dropins.
- **Merging PRs + pushing tags is classifier-blocked for the agent** — Raja runs
  `! gh pr merge <n> --merge --delete-branch` / `! git push origin <tag>` (cd to repo root first —
  worktree cwd breaks `gh pr merge`).

## Status
- **Released v0.1.0** after 3 tag attempts (delete+retag is safe pre-publish per RELEASING.md; attempt 3
  published). Blockers fixed en route, all merged to main (@9059962):
  - **PR #73**: `--version`/`-V`/`--help`/`-h` exit 0 on all three binaries (smoke legs probe them);
    deflaked 6 daemon_cli_lifecycle tests (bounded polling, capture-ready wait; 14/14 under taskset).
  - **PR #74**: reap test treats zombie as terminated (GH fedora container has non-reaping PID 1;
    PDEATHSIG kill verified correct in podman repro — NOT a daemon bug).
  - **PR #75**: aur-publish.sh strips tarballs before `git add` (AUR 488KiB blob limit rejected v0.1.0).
- **First-tag HITL prerequisites ALL DONE** (secrets existed since 07-19; this session added: COPR
  custom package config via copr-cli + fixed COPR_WEBHOOK_URL secret (old one had `<PACKAGE_NAME>`
  placeholder), gh-pages seeded + Pages enabled, stale `raja-dev.me` custom-domain claim removed from
  Anuraj-dev.github.io repo settings (portfolio = Vercel, untouched), first AUR push).
- **Canonical URL: `https://anuraj-dev.github.io/voisu`** (Raja rejected domain/CNAME approaches).
- Landing page (gh-pages/index.html): minimal, distro-tab picker, ~60 words prose — Raja rejected v1 as
  cluttered AI slop; v2 approved. README rewritten (planning-phase text was stale) with install sections.
- Test baseline: 445/0 on main (444 + shortcut token regression test, PR #76). CI green. Benchmark rows
  through 197 (196-197 = PR #76 coder+reviewer; the release-day ~7 dispatches before that still unlogged).

## Architecture map
- Domain, IPC, Transcript decision, FocusProbe trait, ShortcutEvent, KeyDiagnosis, ProviderKeyStatus -> `crates/voisu-core/src/lib.rs`
- Fedora capture/provider/clipboard/portal/libei adapters, GuardedDelivery, SecretToolStore (+retry/cache) -> `crates/voisu-app/src/system.rs`
- Setup wizard -> `crates/voisu-app/src/setup.rs` · Fallback credentials file -> `crates/voisu-app/src/secret_file.rs`
- Focus probes -> `crates/voisu-app/src/focus.rs` · Daemon + shortcut self-heal -> `crates/voisu-app/src/bin/voisu-daemon.rs`
- Config writer -> `crates/voisu-app/src/config.rs` · Dictionary -> `crates/voisu-app/src/dictionary.rs`
- CLI -> `crates/voisu-app/src/bin/voisu.rs` · Overlay -> `bin/voisu-overlay.rs`, `overlay.rs`, `feedback.rs`
- Service lifecycle -> `crates/voisu-app/src/service.rs` · CI -> `.github/workflows/ci.yml`
- Release pipeline -> `.github/workflows/{release.yml,apt-refresh.yml}`, `packaging/ci/`, `packaging/RELEASING.md`
- RPM -> `packaging/voisu.spec`, `build-rpm.sh`, `rpm-lib.sh` · deb -> `[package.metadata.deb]`, `packaging/build-deb.sh`, `packaging/deb/`
- AUR -> `packaging/aur/voisu{,-bin}/` · COPR -> `packaging/build-srpm.sh`, `packaging/copr/`, `.github/workflows/copr-trigger.yml`
- Apt repo -> `packaging/apt/{make-apt-repo.sh,apt-e2e.sh,README.md,voisu-archive-keyring.asc}`
- Landing page -> gh-pages branch `index.html` (worktree often at `../voisu-ghpages`)
- Friends map + tickets -> `.scratch/voisu-friends/` (map.md; issues/01–15; ticket 16 lived only in GH #65)

## Stack & run
- Stack: Rust 2024 + Tokio + serde + zbus 5 + GTK4 (opt-in) + runtime libei + flacenc · Run: `cargo run -p voisu-app --bin voisu-daemon` · Test: `cargo test --workspace`
- Robust test count: `cargo test --workspace 2>&1 | grep -oE "[0-9]+ passed; [0-9]+ failed" | awk '{p+=$1; f+=$3} END {print p, f}'`

## Key decisions (top; full log in decisions.md)
- **Releases are tagged from main ONLY**; pre-publish red gate → fix, delete tag, retag SAME version;
  post-publish → bump version (published bytes immutable). v0.1.0 took 3 attempts, by the book.
- **AUR repos carry only metadata** (PKGBUILD/.SRCINFO/install/license; AUR hard-rejects blobs >488KiB) —
  aur-publish.sh strips tarballs (PR #75).
- **Deflake with root cause, never sleep-padding**: bounded polling with generous ceilings + fast-fail
  try_wait branches; zombie==terminated where init isn't reaping (test-only; daemon PDEATHSIG verified).
- **Apt channel = GitHub Pages self-hosted**, own GPG key; published bytes immutable.
- **No `keyring` crate** — secret-tool boundary (retry + 300s-TTL cache wrap the shell-out).
- **Guarded delivery:** strict stable_id, fail closed.

## Gotchas
- **GlobalShortcuts `session_handle_token` MUST stay the constant `"voisu_session"`** — portal-kde
  persists a kglobalaccel component named after it; any per-process variation re-prompts every start and
  leaks kglobalshortcutsrc sections (PR #76; test pins it). The Delivery/RemoteDesktop session token is
  correctly PID+counter-unique — different mechanism, don't "fix" it. The create/bind request tokens stay
  PID-unique too.
- **AUR pre-receive rejects blobs >488.28KiB** — updpkgsums leaves the downloaded tarball in the staging
  dir; must strip before commit (bit the v0.1.0 publish; fixed PR #75).
- **GH Actions job containers have a non-reaping PID 1** — killed children linger as zombies; /proc
  existence checks must treat state Z as terminated (bit the fedora leg twice; PR #74).
- **daemon_cli_lifecycle + service_cli are timing-sensitive under contended CI** — repro with
  `taskset -c 0` + CPU hogs + RUST_TEST_THREADS=4; different tests fail per run. service_cli sweep in flight.
- **COPR_WEBHOOK_URL-style secrets: verify the URL has real values, not `<PACKAGE_NAME>` placeholders**
  (curl exit 23, zero COPR webhook history). Integrations page has the exact URL.
- **Removing a stale GH Pages custom domain** on one repo un-breaks ALL project pages' URLs; portfolio
  hosting (Vercel/DNS) is independent of GitHub's domain claim. Verify with dig/curl before asserting.
- **RPM `%license` copies files by BASENAME** — use directory form (`%license ring`). Bit PR #72 r1.
- **Ubuntu smoke/build must be ubuntu:26.04**; runners are 24.04 → use containers.
- **Fedora overlay = separate `voisu-overlay` subpackage** (deb/AUR bundle it) — docs must say so (bit the landing page).
- **`voisu --version` etc. must exit 0** — smoke legs probe them (PR #73). Signed-by can take the .asc directly on 26.04.
- **secret-tool lookup: genuine no-match = exit 1 + EMPTY stderr** — verify vs real ksecretd (HITL).
- **`pw-record` is in `pipewire-audio`, `wpctl` in `wireplumber`**.
- **COPR:** SRPM phase networked, mock phase NOT; `rpmbuild -bs` plain (bake %global). %check needs
  TMPDIR=/var/tmp RUST_TEST_THREADS=4 in the spec; run non-root (root breaks auth_set 0o500 test).
- **Check `git branch --show-current` before tree work**; `gh pr merge` fails from a worktree cwd.
- No local clippy/shellcheck/dpkg/lintian — CI is the oracle. `build-*.sh` need a clean COMMITTED
  checkout. Disk tight: `cargo clean` before RPM builds; remove pulled container images (arch image pulled today).
- Use `CONTEXT.md` terms exactly; default builds GTK-free (`--features overlay`).
