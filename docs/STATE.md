# Voisu — State
> Cloud-first Linux desktop dictation app (Fedora KDE Plasma / Wayland) · Last checkpoint: 2026-07-20 (~02:15)

## 🚧 In progress / next
- **Phase B (packaging) underway.** Ticket 09 (GH #41, accounts/keys) CLOSED tonight — full credential
  map in .scratch/voisu-friends/issues/09-packaging-accounts-setup.md. Next: **ticket 10 = cargo-deb
  (GH #42)**, then 11 (AUR), 12 (COPR), 13 (apt repo — decides Pages vs Cloudsmith in-ticket).
- **HITL queue for Raja:** (a) reboot + suspend/resume check of Trigger Key self-heal (PR #61) on next
  installed-binary update — note whether KDE re-prompts after a real shortcut revocation; (b) live KDE
  guarded-mode test (PR #56); (c) GNOME VM visual check of overlay fallback (PR #59); (d) live
  `voisu setup` smoke vs real ksecretd + real provider keys (PR #62); (e) optional keyring probe kit
  install (.scratch/voisu-friends/assets/06-keyring-probe/INSTALL.md); (f) on next RPM install delete
  `~/.config/systemd/user/voisu{,-overlay}.service.d/sandbox-validation.conf`; (g) **enable AUR TOTP
  the moment aurweb ships 2FA** (doesn't exist upstream as of 2026-07-20).

## Status
- **Ticket 09 closed (2026-07-20, HITL live session):** GPG apt-signing key
  `4149EE3868B36B6007592966D08BCFDC34125B28` (ed25519, passphrased, offline backup verified, public key
  at packaging/apt/voisu-archive-keyring.asc — NOT yet committed); FAS+COPR `anuraj-dev/voisu` (ID
  246563, F43/44 x86_64, net disabled); AUR `anuraj-dev` (dedicated no-passphrase deploy key
  ~/.ssh/keys/aur_voisu, SSH auth verified; voisu/voisu-bin free — claim = ticket 11's first push);
  GH secrets: AUR_SSH_PRIVATE_KEY, GPG_PRIVATE_KEY, GPG_PASSPHRASE, COPR_WEBHOOK_URL. No secret value
  recorded anywhere; passphrases live only in Raja's password manager.
- Ticket 07 closed via PR #62; ticket 06 closed (ksecretd probe); GH #60 fixed via PR #61. Phase A done.
- Test baseline: **431 passed / 0 failed**, both default and `--features voisu-app/overlay`.
- CI flake #58 family: rerun once, never twice on the same PR.
- `docs/model-benchmark.md` rows through 173.
- **ROUTING (Raja):** Sol/cladex = REVIEWS ONLY (first high, re-reviews medium); ALL implementation →
  Opus 4.8 high (architectural → Fable medium); Sol dispatch fails → retry Sol once → Fable subagent;
  2 failed review rounds → discard implementer → Fable.

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
- Service lifecycle -> `crates/voisu-app/src/service.rs` · RPM/CI -> `packaging/`, `.github/workflows/ci.yml`
- Apt-repo public signing key -> `packaging/apt/voisu-archive-keyring.asc`
- Friends map + per-ticket resolutions -> `.scratch/voisu-friends/` (map.md; issues/01–15; assets/)

## Stack & run
- Stack: Rust 2024 + Tokio + serde + zbus 5 + GTK4 (opt-in) + runtime libei + flacenc · Run: `cargo run -p voisu-app --bin voisu-daemon` · Test: `cargo test --workspace`
- Robust test count: `cargo test --workspace 2>&1 | grep -oE "[0-9]+ passed; [0-9]+ failed" | awk '{p+=$1; f+=$3} END {print p, f}'`

## Key decisions (top; full log in decisions.md)
- **Packaging credentials architecture (ticket 09):** GPG key passphrased (CI signs via loopback pinentry,
  GPG_PASSPHRASE secret); AUR deploy key deliberately passphrase-free (scope-limited, CI auto-push);
  secret VALUES never in repo/docs/output — locations only. AUR 2FA unavailable upstream → compensating
  controls + queued HITL.
- **Trigger Key permanence contract (PR #61):** only a refused bind (portal response 1) retires the
  listener; Session.Closed and stream death are recoverable.
- **No `keyring` crate (PR #62):** secret-tool boundary instead — both crate backends drag duplicate
  D-Bus stacks next to zbus 5.
- **Codex = reviews only; Claude implements** (Raja 2026-07-19); Sol retry-once-then-Fable.
- **Guarded delivery**: strict stable_id-only match; fail closed on unknown (PR #56).

## Gotchas
- **COPR project names are case-sensitive and un-renameable** — a stray `Voisu` had to be deleted and
  recreated lowercase. COPR builders have NO network (vendor crates); auto-rebuild flag is per-package.
- **cladex JSON output carries ONLY the final message** — review prompts must demand self-contained
  final findings; dispatch dies silently sometimes → retry-once-then-Fable.
- **Resumed subagents may arm a Monitor and stall** — resumed-agent prompts must forbid monitor-waits.
- **No local clippy (no rustup)** — CI is the only clippy oracle.
- Disk tight (~7 GB free). `cargo clean` before RPM builds; `TMPDIR=/var/tmp RUST_TEST_THREADS=4`.
- **Don't switch branches while a tree-using agent runs.**
- gh's GraphQL PR-edit is broken on this repo — PATCH PR bodies via `gh api -X PATCH …/pulls/<n> -F body=@file`.
- `packaging/build-rpm.sh` needs a clean COMMITTED checkout.
- Use `CONTEXT.md` terms exactly; default builds GTK-free (`--features overlay` for the Overlay).
- AUR pacman-captcha on signup: solve via throwaway `podman run --rm archlinux` (image removed after — disk).
