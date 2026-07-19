# Voisu — State
> Cloud-first Linux desktop dictation app (Fedora KDE Plasma / Wayland) · Last checkpoint: 2026-07-20 (early AM)

## 🚧 In progress / next
- **PHASE A COMPLETE** (friends map, .scratch/voisu-friends/, GH #32). Next: **phase B**.
  1. **Ticket 09 (GH #41) — Raja's guided accounts checklist: STARTING NOW** in a fresh Fable-medium
     session (handoff prompt delivered; Raja is live for HITL).
  2. Then packaging tickets 10–15 (GH #42–47); ticket 10 = cargo-deb (GH #42) is unblocked.
- **HITL queue for Raja:** (a) reboot + suspend/resume check of the Trigger Key self-heal (PR #61) on the
  next installed-binary update — also note whether KDE re-prompts after a real shortcut revocation;
  (b) live KDE guarded-mode test (PR #56); (c) GNOME VM visual check of overlay fallback (PR #59);
  (d) live `voisu setup` smoke vs real ksecretd + real Deepgram/Groq keys (PR #62);
  (e) optional: install keyring probe kit (.scratch/voisu-friends/assets/06-keyring-probe/INSTALL.md),
  reboot, check `journalctl --user -t voisu-keyring-probe -b`; (f) on next RPM install delete
  `~/.config/systemd/user/voisu{,-overlay}.service.d/sandbox-validation.conf`.

## Status
- **Ticket 06 (keyring probe) closed 2026-07-19:** Secret Service = ksecretd (PAM-launched, NOT kwalletd6),
  reachable AND unlocked ~29s before daemon start, 45–48ms round trips, zero prompts. Asset:
  .scratch/voisu-friends/assets/06-keyring-service-probe.md.
- **GH #60 fixed via PR #61 (merged):** Trigger Key now self-heals across reboot/suspend — backoff rebind
  (1s→30s cap, indefinite), Session.Closed recoverable, only portal response 1 (refusal) retires it.
- **Ticket 07 closed via PR #62 (merged, 5 review rounds):** `voisu setup` wizard (injected-IO, signal-safe
  hidden entry), keyring storage on the secret-tool boundary (NO keyring crate — dep-stack deviation
  reviewer-endorsed), flock-serialized loud 0600 fallback, real plaintext→keyring migration with
  content-aware prune classification, `voisu doctor` provider-key classification incl. malformed-env FAIL.
  Deferred follow-up (in PR #62 body): wizard-scale keyring deadline vs 2s PROCESS_DEADLINE.
- Test baseline: **431 passed / 0 failed**, both default and `--features voisu-app/overlay`.
- CI flake #58 family: managed_service_lifecycle + service_cli batch (5 local failures once, clean rerun)
  + "controlled processing panic" unit test — all rerun-once, never twice on the same PR.
- `docs/model-benchmark.md` rows through 172.
- **ROUTING (Raja):** Sol/cladex = REVIEWS ONLY (first high, re-reviews medium); ALL implementation →
  Opus 4.8 high (architectural → Fable medium); Sol dispatch fails → retry Sol once → Fable subagent;
  2 failed review rounds → discard implementer → Fable.

## Architecture map
- Domain, IPC, Transcript decision, FocusProbe trait, ShortcutEvent, KeyDiagnosis, ProviderKeyStatus -> `crates/voisu-core/src/lib.rs`
- Fedora capture/provider/clipboard/portal/libei adapters, GuardedDelivery, SecretToolStore (retry+diagnose+replace), portal_request permanence -> `crates/voisu-app/src/system.rs`
- Setup wizard (WizardIo/SecretStore/KeyValidator traits, EchoGuard) -> `crates/voisu-app/src/setup.rs`
- Fallback credentials file (CredentialsLock flock, RemoveError content-aware classification) -> `crates/voisu-app/src/secret_file.rs`
- Focus probes (KWin script/D-Bus push + sender auth, hyprctl, Null) -> `crates/voisu-app/src/focus.rs`
- Recording/replay supervision, shortcut_listener self-heal (RebindBackoff, VOISU_TEST_SHORTCUT_REBIND_*_MS seams) -> `crates/voisu-app/src/bin/voisu-daemon.rs`
- Persisted config (both-key-preserving writer) -> `crates/voisu-app/src/config.rs`
- Dictionary (flock-serialized) + keyterm cap -> `crates/voisu-app/src/dictionary.rs`
- Public CLI (`voisu setup|doctor|deepgram|delivery|dictionary|history|auth`) -> `crates/voisu-app/src/bin/voisu.rs`
- GTK Overlay + pure controllers -> `crates/voisu-app/src/bin/voisu-overlay.rs`, `overlay.rs`, `feedback.rs`
- Service lifecycle -> `crates/voisu-app/src/service.rs` · RPM/CI -> `packaging/`, `.github/workflows/ci.yml`
- Friends map + per-ticket resolutions -> `.scratch/voisu-friends/` (map.md; issues/01–15; assets/)

## Stack & run
- Stack: Rust 2024 + Tokio + serde + zbus 5 + GTK4 (opt-in) + runtime libei + flacenc · Run: `cargo run -p voisu-app --bin voisu-daemon` · Test: `cargo test --workspace`
- Robust test count: `cargo test --workspace 2>&1 | grep -oE "[0-9]+ passed; [0-9]+ failed" | awk '{p+=$1; f+=$3} END {print p, f}'`

## Key decisions (top; full log in decisions.md)
- **Trigger Key permanence contract (PR #61):** only a refused bind (portal response 1) retires the
  listener; Session.Closed and stream death are recoverable; the no-reprompt bound is the portal's to
  enforce, not the listener's.
- **No `keyring` crate (PR #62):** both its Secret Service backends drag duplicate D-Bus stacks next to
  zbus 5; secret-tool boundary delivers the same intent with zero new deps.
- **Fallback-file honesty (PR #62):** prune outcome keyed on target-provider line content —
  gone/survived/unverifiable — classified once in FileSecretStore::remove; all mutations flock-serialized.
- **Codex = reviews only; Claude implements** (Raja 2026-07-19); Sol retry-once-then-Fable (2026-07-19).
- **Guarded delivery**: strict stable_id-only match; fail closed on unknown (PR #56).

## Gotchas
- **cladex JSON output carries ONLY the final message** — every review prompt must demand self-contained
  final findings. One dispatch died silently (proxy start/stop only) — hence retry-once-then-Fable.
- **Resumed subagents may arm a Monitor and stall** — dispatch prompts for resumed agents should say
  "run everything synchronously; do not wait on monitors" (one Fable fix round stalled this way).
- **No local clippy (no rustup)** — CI is the only clippy oracle; keep diffs lint-clean by inspection.
- Disk tight (~7 GB free). `cargo clean` before RPM builds; `TMPDIR=/var/tmp RUST_TEST_THREADS=4`.
- **Don't switch branches while a tree-using agent runs.**
- CI/local flake family (#58): rerun once; twice on the same PR → stop and investigate.
- gh's GraphQL PR-edit is broken on this repo (deprecated Projects-classic) — PATCH PR bodies via
  `gh api -X PATCH repos/Anuraj-dev/voisu/pulls/<n> -F body=@file`.
- `packaging/build-rpm.sh` needs a clean COMMITTED checkout · COPR builders have NO network (vendor crates).
- Use `CONTEXT.md` terms exactly; default builds GTK-free (`--features overlay` for the Overlay).
