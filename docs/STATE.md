# Voisu — State
> Cloud-first Linux desktop dictation app (Fedora KDE Plasma / Wayland) · Last checkpoint: 2026-07-20 (~17:50)

## 🚧 In progress / next
- **Ticket 12 (COPR, GH #44) mid-fix-round on `feat/copr-channel` / PR #66.** Sol's first review (high)
  returned 15 findings (4 major). Orchestrator direction on the majors: (1) make-srpm.sh self-pins tag
  builds (fetch tags+history, checkout `v<cargo-version>` commit when it exists; no COPR API token);
  (2) ONE Release policy — all pre-release `0.<count>.<ct>.git<sha>`, tagged plain `N` from a committed
  packaging value (build-rpm.sh change in scope); (3) %check exports TMPDIR=/var/tmp
  RUST_TEST_THREADS=4; (4) version derived from cargo metadata, validated == spec, respin via the
  committed release-number value. An Opus fix agent (2nd — first was killed mid-run by Raja; its main
  fix commit `8297d04` survived locally) is verifying/finishing/pushing. **Next step: Sol re-review at
  medium once it pushes, then merge on CI green.** Then ticket 13 (apt repo — Pages vs Cloudsmith
  decided in-ticket), 14 (release workflow + CI smoke), 15 (live desktop validation, HITL), 16 below.
- **Ticket 16 filed (GH #65, `.scratch/voisu-friends/issues/16-license-tree-deb-rpm.md`):** ring
  license-tree compliance gap in deb + RPM (renamed paths dangling from ring's manifest; once_cell +
  fiat texts missing). Fix mirrors packaging/aur/voisu/PKGBUILD. Land before first tagged release.
- **Overlay audio waveform (not built):** approved spec at `docs/specs/2026-07-20-overlay-audio-waveform.md`
  — live bar meter during Recording only; §11 verification pass required before code.
- **HITL queue for Raja:** (a) reboot + suspend/resume check of Trigger Key self-heal (PR #61);
  (b) live KDE guarded-mode test (PR #56); (c) GNOME VM visual check (PR #59); (d) live `voisu setup`
  smoke vs real ksecretd (PR #62); (e) optional keyring probe kit; (f) delete
  `~/.config/systemd/user/voisu{,-overlay}.service.d/sandbox-validation.conf` on next RPM install;
  (g) AUR TOTP when aurweb ships 2FA; (h) **first AUR push** (deploy key, ticket 11 did no remote);
  (i) **COPR side (ticket 12):** configure custom-source package (bootstrap → packaging/copr/make-srpm.sh,
  script-builddeps "git cargo rust rust-std-static", resultdir _copr_srpm), enable per-package
  webhook-rebuild, confirm project webhook URL == COPR_WEBHOOK_URL secret, optional first build;
  (j) **one-time dev-RPM downgrade** (`rpm -Uvh --oldpackage`/dnf downgrade) after the Release-policy
  change — installed `1.git<sha>` outranks the new `0.<count>...` scheme.
- **Merging PRs is classifier-blocked for the agent** — Raja merges via `! gh pr merge <n> --merge --delete-branch`.

## Status
- **Tickets 10 (deb, PR #63) and 11 (AUR, PR #64) MERGED.** Ticket 11 closed with Sol zero-findings
  approve. AUR deps corrected: pipewire-audio (owns pw-record), wireplumber (wpctl), libxkbcommon
  runtime, checkdepends python+dbus; `options=('!lto')` (ring). namcap clean.
- **Ticket 12 round 0 (PR #66, unmerged):** cargo-vendor SRPM (Source1 vendor tarball, offline %build),
  packaging/build-srpm.sh + packaging/copr/make-srpm.sh (COPR custom-source; SRPM phase networked,
  mock phase not), .github/workflows/copr-trigger.yml (v* tag → webhook), spec voisu_release macro.
  Offline rebuild proven in fedora:43 --network=none; found+fixed --define non-persistence
  (%global baking). 15 review findings in flight (above).
- Phase A complete (01–08); ticket 09 accounts/keys live. Test baseline: **431 passed / 0 failed**.
- `docs/model-benchmark.md` rows through 179. CI flake #58 family: rerun once, never twice.
- **ROUTING (Raja):** Sol/cladex = REVIEWS ONLY (first high, re-reviews medium); ALL implementation →
  Opus 4.8 high (architectural → Fable medium); 2 failed review rounds → discard implementer → Fable/driver.

## Architecture map
- Domain, IPC, Transcript decision, FocusProbe trait, ShortcutEvent, KeyDiagnosis, ProviderKeyStatus -> `crates/voisu-core/src/lib.rs`
- Fedora capture/provider/clipboard/portal/libei adapters, GuardedDelivery, SecretToolStore -> `crates/voisu-app/src/system.rs`
- Setup wizard -> `crates/voisu-app/src/setup.rs` · Fallback credentials file -> `crates/voisu-app/src/secret_file.rs`
- Focus probes -> `crates/voisu-app/src/focus.rs` · Daemon + shortcut self-heal -> `crates/voisu-app/src/bin/voisu-daemon.rs`
- Config writer -> `crates/voisu-app/src/config.rs` · Dictionary -> `crates/voisu-app/src/dictionary.rs`
- CLI -> `crates/voisu-app/src/bin/voisu.rs` · Overlay -> `bin/voisu-overlay.rs`, `overlay.rs`, `feedback.rs`
- Service lifecycle -> `crates/voisu-app/src/service.rs` · CI -> `.github/workflows/ci.yml`
- RPM -> `packaging/voisu.spec`, `build-rpm.sh` · deb -> `[package.metadata.deb]`, `packaging/build-deb.sh`, `packaging/deb/`
- AUR -> `packaging/aur/voisu{,-bin}/` · COPR (PR #66) -> `packaging/build-srpm.sh`, `packaging/copr/`, `.github/workflows/copr-trigger.yml`
- Apt-repo public signing key -> `packaging/apt/voisu-archive-keyring.asc`
- Friends map + tickets -> `.scratch/voisu-friends/` (map.md; issues/01–16; assets/)

## Stack & run
- Stack: Rust 2024 + Tokio + serde + zbus 5 + GTK4 (opt-in) + runtime libei + flacenc · Run: `cargo run -p voisu-app --bin voisu-daemon` · Test: `cargo test --workspace`
- Robust test count: `cargo test --workspace 2>&1 | grep -oE "[0-9]+ passed; [0-9]+ failed" | awk '{p+=$1; f+=$3} END {print p, f}'`

## Key decisions (top; full log in decisions.md)
- **RPM Release policy (ticket 12, in review):** all pre-release `0.<count>.<ct>.git<sha>`, tagged plain
  `N` from a committed value — dev and channel builds order correctly; one-time --oldpackage on Raja's box.
- **COPR tag provenance without an API token:** make-srpm.sh self-pins by checking out the `v<version>`
  tag commit when present; webhook stays the only credential. Token-based pinned builds = ticket 14 option.
- **deb must be built on Ubuntu (ticket 10):** $auto/dpkg-shlibdeps encodes real ABI floors; detect-and-fail.
- **Debian dev-version ordering = commit count, not timestamp** (clock skew; shallow clones refused).
- **No `keyring` crate (PR #62):** secret-tool boundary. · **Guarded delivery:** strict stable_id, fail closed.

## Gotchas
- **`pw-record` is in `pipewire-audio`, `wpctl` in `wireplumber`** — base `pipewire` alone fails at first Recording.
- **ring fails to link under LTO** — Arch needs `options=('!lto')`. **ring license tree** must keep upstream paths (ticket 16).
- **COPR:** SRPM phase has network, mock phase does NOT (vendor everything); project names case-sensitive,
  un-renameable; auto-rebuild flag per-package; plain `rpmbuild -bs` — no --define (bake %global).
- **rpmbuild %check needs TMPDIR=/var/tmp RUST_TEST_THREADS=4** — must be exported IN the spec, mock won't inherit.
- **Run %check/mock as non-root** — root falsely passes the 0o500-refuses-writes test.
- **cladex JSON carries ONLY the final message** — reviews must restate all findings in the final message.
- **Subagents stall on background waiters/monitors** — dispatch prompts must mandate foreground-only; it
  happened AGAIN this session (ticket 12 agent, twice).
- **No local clippy / shellcheck** — CI is the oracle. Don't switch branches while a tree-using agent runs.
- gh GraphQL PR-edit broken — PATCH via `gh api`. `build-rpm.sh`/`build-deb.sh`/`build-srpm.sh` need a clean
  COMMITTED checkout. Disk: `cargo clean` before RPM builds; `TMPDIR=/var/tmp RUST_TEST_THREADS=4`;
  remove pulled container images after packaging work (~26 GB free as of last cleanup).
- Use `CONTEXT.md` terms exactly; default builds GTK-free (`--features overlay` for the Overlay).
