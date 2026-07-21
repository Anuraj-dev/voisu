# Voisu — State
> Cloud-first Linux desktop dictation app (Fedora KDE Plasma / Wayland) · Last checkpoint: 2026-07-21 (~10:25)

## 🚧 In progress / next
- **Phase B repo work COMPLETE.** This session merged: PR #70 (secret-lookup retry+cache, GH #69),
  **PR #71 = ticket 14** (on-tag release pipeline + smoke gates), **PR #72 = ticket 16** (ring license
  trees deb+RPM, GH #65). Main @ a68e0c9. The first tagged release (v0.1.0) is now blocked ONLY on
  Raja's HITL prerequisites below — see `packaging/RELEASING.md` for the full runbook.
- **NEXT TICKET: 15 (live desktop validation, `.scratch/voisu-friends/issues/15-*.md`)** — HITL-heavy;
  driver orchestrates the checklist with Raja at the desk. No further AFK implementation tickets queued.
- **Sol/cladex budget: 0 (spent on PR #71 r1).** ALL reviews → Opus 4.8 subagent high until Raja
  re-enables codex. Pattern that worked this session: Opus impl → Opus fresh-agent review → resumed-agent
  fix → driver delta-verify inline (no third review for mechanical deltas).
- **Two questions parked for Raja:** (a) Sol/Terra/Luna vs Opus benchmark FINAL REPORT — write now or
  wait for more Opus-review data? (rows through 195); (b) quality-gate transcript drops (~11 in 4 days,
  by design) — product/tuning decision (degraded best-effort delivery?) — ASK before speccing.
- **HITL queue for Raja (first-tag prerequisites first):** (a) create `AUR_SSH_PRIVATE_KEY` GH secret +
  ensure AUR repos exist (steps in packaging/RELEASING.md); (b) apt-repo one-time setup (seed orphan
  gh-pages, enable Pages, fingerprint smoke — packaging/apt/README.md); (c) COPR custom-source package
  config + webhook-rebuild flag (ticket 12 notes in decisions.md); (d) first AUR push (deploy key);
  (e) rebuild + upgrade installed RPM to current main (one-time `dnf downgrade`/`--oldpackage`;
  installed 0.1.0-1.git58a607f misses PR #61 + #70); (f) reboot+suspend Trigger-Key self-heal check
  (PR #61); (g) live KDE guarded-mode test (PR #56); (h) GNOME VM visual check (PR #59); (i) `voisu
  setup` smoke vs real ksecretd (PR #62) — doubles as the PR #70 empty-stderr verification; (j) AUR
  TOTP when aurweb ships 2FA; (k) delete sandbox-validation.conf dropins on next RPM install.
- **Merging PRs is classifier-blocked for the agent** — Raja merges via `! gh pr merge <n> --merge --delete-branch`.

## Status
- **Phase B packaging DONE: tickets 10–14 + 16 all merged** (deb #63, AUR #64, COPR #66, apt #68,
  release workflow #71, license trees #72). Phase A complete (01–08); ticket 09 accounts/keys live.
- **Release pipeline (ticket 14):** `v<semver>` tag → validate (strict regex + tag-must-be-ancestor-of-
  main) → build deb in ubuntu:26.04 container + flat release tarball → smoke matrix (fedora:latest /
  ubuntu:26.04 / archlinux source / archlinux voisu-bin, each installing the fresh artifacts through the
  real package-manager path) → publish (apt→gh-pages real-key signed, AUR vercmp-guarded, GH Release);
  COPR self-triggers. Weekly `apt-refresh.yml` re-signs Release (30d Valid-Until). Secrets:
  GPG_PRIVATE_KEY/GPG_PASSPHRASE (exist), AUR_SSH_PRIVATE_KEY (Raja must create).
- **License compliance (ticket 16):** deb+RPM ship ring's upstream-path 6-text tree byte-identical to
  ring 0.17.14 (matches AUR + release tarball); DEP-5 rewritten; RPM uses `%license ring` directory form.
- Test baseline: **439/0 on main.** CI green. `docs/model-benchmark.md` rows through 195.
- PR #70 postscript: cache is the load-bearing half; follow-ups (ksecretd empty-stderr verify = HITL i;
  optional cold-daemon retry variant) are non-blocking.

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
- Friends map + tickets -> `.scratch/voisu-friends/` (map.md; issues/01–15; ticket 16 lived only in GH #65)

## Stack & run
- Stack: Rust 2024 + Tokio + serde + zbus 5 + GTK4 (opt-in) + runtime libei + flacenc · Run: `cargo run -p voisu-app --bin voisu-daemon` · Test: `cargo test --workspace`
- Robust test count: `cargo test --workspace 2>&1 | grep -oE "[0-9]+ passed; [0-9]+ failed" | awk '{p+=$1; f+=$3} END {print p, f}'`

## Key decisions (top; full log in decisions.md)
- **Releases are tagged from main ONLY** (ticket 14): validate job enforces merge-base ancestry, fail closed.
- **Smoke gates install locally built artifacts** through real package-manager paths (channels can't be
  live pre-publish); equal-pkgver AUR publish is a no-op BY DESIGN — packaging-only fixes need a patch bump.
- **Apt channel = GitHub Pages self-hosted**, apt-ftparchive, own GPG key; published bytes immutable.
- **RPM Release policy:** pre-release `0.<count>.<ct>.git<sha>`, tagged plain `N` from committed
  packaging/rpm-release; one-time --oldpackage on Raja's box.
- **No `keyring` crate** — secret-tool boundary (retry + 300s-TTL cache wrap the shell-out).
- **Guarded delivery:** strict stable_id, fail closed.

## Gotchas
- **RPM `%license`/`%doc` copies listed FILES by BASENAME** — flattens trees and silently collides
  same-named files (rpmbuild still exits 0). Use the DIRECTORY form (`%license ring`). Bit PR #72 r1.
- **`makepkg --needed` IS valid** (documented pass-to-pacman option) — an Opus reviewer flagged it as
  release-blocking; man page refuted. Verify "invalid option" claims against makepkg(8) before acting.
- **Ubuntu smoke/build must be ubuntu:26.04** (support matrix; 24.04 lacks gtk4-layer-shell; runners are 24.04 → use containers).
- **Check `git branch --show-current` before tree work**; never switch branches while a tree-using agent
  runs; driver docs commits to main via TEMP WORKTREE when the tree sits on a feature branch.
- **secret-tool lookup: genuine no-match = exit 1 + EMPTY stderr** — PR #70 classification relies on it; verify vs real ksecretd (HITL i).
- **`pw-record` is in `pipewire-audio`, `wpctl` in `wireplumber`** — base `pipewire` alone fails at first Recording.
- **ring fails to link under LTO** (Arch `!lto`); ring license tree must keep upstream paths (done, ticket 16).
- **COPR:** SRPM phase networked, mock phase NOT; names case-sensitive; plain `rpmbuild -bs` (bake %global).
- **rpmbuild %check needs TMPDIR=/var/tmp RUST_TEST_THREADS=4 exported IN the spec; run non-root.**
- **cladex JSON carries ONLY the final message** — demand a self-contained final message (n/a while budget 0).
- **Subagents stall on background waiters** — dispatch prompts must mandate foreground-only (clean this session).
- No local clippy/shellcheck/dpkg/lintian — CI is the oracle. gh GraphQL PR-edit broken — PATCH via `gh api`.
- `build-*.sh` need a clean COMMITTED checkout. Disk tight: `cargo clean` before RPM builds; remove pulled container images.
- Use `CONTEXT.md` terms exactly; default builds GTK-free (`--features overlay` for the Overlay).
