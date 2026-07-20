# Voisu — State
> Cloud-first Linux desktop dictation app (Fedora KDE Plasma / Wayland) · Last checkpoint: 2026-07-20 (~12:57)

## 🚧 In progress / next
- **Phase B (packaging).** Ticket 10 (cargo-deb, GH #42) MERGED (PR #63, commit c229ff4).
  **Ticket 11 (AUR, GH #43) is mid-review on branch `feat/aur-packages` / PR #64** — Sol's first review
  (9 findings) is fully fixed and pushed (`a7d2f6d`); **the very next step is the Sol re-review at
  medium effort**, then merge on CI green. Then 12 (COPR), 13 (apt repo — Pages vs Cloudsmith decided
  in-ticket), 14 (release workflow + CI smoke), 15 (live desktop validation, mostly HITL).
- **NEW follow-up ticket needed (not yet filed): third-party license trees in the deb AND rpm.**
  Ticket 11 fixed this for AUR only. The deb ships ring's texts under renamed `LICENSE.ring-*` paths
  while ring's own manifest cross-references upstream names (dangling refs), and both deb and RPM omit
  `src/polyfill/once_cell/LICENSE-{APACHE,MIT}` and `third_party/fiat/LICENSE` entirely — a real
  distribution-compliance gap, not cosmetics. Fix mirrors AUR: preserve the upstream tree, add the
  three missing files, update the DEP-5 `copyright` stanzas / `%license` list.
- **Overlay audio waveform (not built):** approved spec at `docs/specs/2026-07-20-overlay-audio-waveform.md`
  — live bar meter during Recording only. Implementing agent MUST do the §11 verification pass before
  writing code (touches the audio capture path).
- **HITL queue for Raja:** (a) reboot + suspend/resume check of Trigger Key self-heal (PR #61) on next
  installed-binary update — note whether KDE re-prompts after a real shortcut revocation; (b) live KDE
  guarded-mode test (PR #56); (c) GNOME VM visual check of overlay fallback (PR #59); (d) live
  `voisu setup` smoke vs real ksecretd + real provider keys (PR #62); (e) optional keyring probe kit
  install (.scratch/voisu-friends/assets/06-keyring-probe/INSTALL.md); (f) on next RPM install delete
  `~/.config/systemd/user/voisu{,-overlay}.service.d/sandbox-validation.conf`; (g) **enable AUR TOTP
  the moment aurweb ships 2FA** (doesn't exist upstream as of 2026-07-20); (h) **first AUR push is HITL**
  with the deploy key — ticket 11 deliberately performed no AUR remote interaction.
- **Merging is blocked for the agent** — the sandbox classifier denies `gh pr merge`. Raja merges via
  `! gh pr merge <n> --merge --delete-branch`, or adds a Bash permission rule to delegate it.

## Status
- **Ticket 10 closed (PR #63, 4 review rounds):** `[package.metadata.deb]` + `packaging/build-deb.sh` +
  `packaging/deb/` (print-only postinst/postrm, DEP-5 copyright, ring license texts). `$auto` shlibdeps
  dependency discovery (so the deb MUST be built on Ubuntu — script detects missing dpkg-shlibdeps and
  fails loudly), monotonic dev versions `<base>~git<count>.<ct>.<sha>-1` with shallow-clone refusal,
  release path gated on a `v<semver>` tag, rm -rf confined to a non-symlinked `$root/dist/`.
  Verified in ubuntu:24.10: install smoke, `systemd-analyze verify`, lintian **0 tags**.
- **Ticket 11 implemented, awaiting re-review (PR #64):** source `voisu` + `voisu-bin` PKGBUILDs,
  both `.SRCINFO`, print-only `.install` scriptlets, ring license tree at upstream paths.
  Deps corrected after review: added `pipewire-audio` (owns `pw-record` — base `pipewire` does NOT),
  `wireplumber` (owns `wpctl`), `libxkbcommon` moved to runtime; `checkdepends=(python dbus)`.
  `options=('!lto')` required — ring fails to link under LTO. namcap clean on both PKGBUILDs.
- Phase A complete (tickets 01–08). Test baseline: **431 passed / 0 failed**, default and
  `--features voisu-app/overlay`.
- CI flake #58 family: rerun once, never twice on the same PR.
- `docs/model-benchmark.md` rows through 177.
- **ROUTING (Raja):** Sol/cladex = REVIEWS ONLY (first high, re-reviews medium); ALL implementation →
  Opus 4.8 high (architectural → Fable medium); Sol dispatch fails → retry Sol once → Fable subagent;
  2 failed review rounds → discard implementer → Fable/driver.

## Architecture map
- Domain, IPC, Transcript decision, FocusProbe trait, ShortcutEvent, KeyDiagnosis, ProviderKeyStatus -> `crates/voisu-core/src/lib.rs`
- Fedora capture/provider/clipboard/portal/libei adapters, GuardedDelivery, SecretToolStore, portal_request permanence -> `crates/voisu-app/src/system.rs`
- Setup wizard (WizardIo/SecretStore/KeyValidator, EchoGuard) -> `crates/voisu-app/src/setup.rs`
- Fallback credentials file (CredentialsLock flock, content-aware RemoveError) -> `crates/voisu-app/src/secret_file.rs`
- Focus probes (KWin script/D-Bus push + sender auth, hyprctl, Null) -> `crates/voisu-app/src/focus.rs`
- Recording/replay supervision, shortcut_listener self-heal (RebindBackoff) -> `crates/voisu-app/src/bin/voisu-daemon.rs`
- Persisted config (both-key-preserving writer) -> `crates/voisu-app/src/config.rs`
- Dictionary (flock-serialized) + keyterm cap -> `crates/voisu-app/src/dictionary.rs`
- Public CLI (`voisu setup|doctor|deepgram|delivery|dictionary|history|auth`) -> `crates/voisu-app/src/bin/voisu.rs`
- GTK Overlay + pure controllers -> `crates/voisu-app/src/bin/voisu-overlay.rs`, `overlay.rs`, `feedback.rs`
- Service lifecycle -> `crates/voisu-app/src/service.rs` · CI -> `.github/workflows/ci.yml`
- RPM -> `packaging/voisu.spec`, `build-rpm.sh` · deb -> `crates/voisu-app/Cargo.toml` `[package.metadata.deb]`,
  `packaging/build-deb.sh`, `packaging/deb/` · AUR -> `packaging/aur/voisu{,-bin}/` (PR #64)
- Apt-repo public signing key -> `packaging/apt/voisu-archive-keyring.asc`
- Friends map + per-ticket resolutions -> `.scratch/voisu-friends/` (map.md; issues/01–15; assets/)

## Stack & run
- Stack: Rust 2024 + Tokio + serde + zbus 5 + GTK4 (opt-in) + runtime libei + flacenc · Run: `cargo run -p voisu-app --bin voisu-daemon` · Test: `cargo test --workspace`
- Robust test count: `cargo test --workspace 2>&1 | grep -oE "[0-9]+ passed; [0-9]+ failed" | awk '{p+=$1; f+=$3} END {print p, f}'`

## Key decisions (top; full log in decisions.md)
- **deb must be built on Ubuntu (ticket 10):** `$auto`/dpkg-shlibdeps encodes the real GLIBC + GTK
  version floors; a Fedora-built binary would encode the wrong floor, so a hidden container build was
  rejected in favour of detect-and-fail. CI builds it on Ubuntu (ticket 14).
- **Debian dev-version ordering key is the commit COUNT, not the timestamp** — committer clocks aren't
  monotonic across rebases/imports; the count is, given full history (shallow clones refused).
- **Packaging credentials architecture (ticket 09):** GPG key passphrased (CI signs via loopback
  pinentry); AUR deploy key deliberately passphrase-free; secret VALUES never in repo/docs/output.
- **Trigger Key permanence contract (PR #61):** only a refused bind (portal response 1) retires the
  listener; Session.Closed and stream death are recoverable.
- **No `keyring` crate (PR #62):** secret-tool boundary instead — both crate backends drag duplicate
  D-Bus stacks next to zbus 5.
- **Guarded delivery**: strict stable_id-only match; fail closed on unknown (PR #56).

## Gotchas
- **`pw-record` is in `pipewire-audio`, `wpctl` is in `wireplumber`** — depending on base `pipewire`
  alone installs fine and then fails at the first Recording. Caught by review, not by testing.
- **ring does not link under LTO** — Arch PKGBUILD needs `options=('!lto')` (undefined `ring_core_*`).
- **COPR project names are case-sensitive and un-renameable.** COPR builders have NO network (vendor
  crates); auto-rebuild flag is per-package.
- **cladex JSON output carries ONLY the final message** — review prompts must demand self-contained
  final findings; dispatch dies silently sometimes → retry-once-then-Fable.
- **Resumed subagents may arm a Monitor and stall** — resumed-agent prompts must forbid monitor-waits.
- **No local clippy (no rustup)** — CI is the only clippy oracle. No shellcheck locally either.
- Disk was tight; ticket 11 cleanup left ~18 GB free. Still `cargo clean` before RPM builds and use
  `TMPDIR=/var/tmp RUST_TEST_THREADS=4`; always remove pulled container images after packaging work.
- **Don't switch branches while a tree-using agent runs.**
- gh's GraphQL PR-edit is broken on this repo — PATCH PR bodies via `gh api -X PATCH …/pulls/<n> -F body=@file`.
- `packaging/build-rpm.sh` needs a clean COMMITTED checkout (so does `build-deb.sh`).
- Use `CONTEXT.md` terms exactly; default builds GTK-free (`--features overlay` for the Overlay).
- AUR pacman-captcha on signup: solve via throwaway `podman run --rm archlinux` (image removed after).
