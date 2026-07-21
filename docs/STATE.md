# Voisu — State
> Cloud-first Linux desktop dictation app (Fedora KDE Plasma / Wayland) · Last checkpoint: 2026-07-21 (~08:35)

## 🚧 In progress / next
- **PR #70 (fix/secret-lookup-retry, GH #69) APPROVED + CI GREEN — awaiting Raja's merge**
  (`! gh pr merge 70 --merge --delete-branch`). Mid-session `secret-tool lookup denied` fix:
  stderr-classified bounded retry [100,250]ms + 300s-TTL in-process session credential cache (cache is
  the load-bearing half for the warm-daemon incident). Opus r1 (high): APPROVE, 0 major / 3 minor
  (pre-existing or documented) / 3 nit; suite 439/0. Two non-blocking follow-ups: (a) verify the
  empty-stderr-on-no-match assumption against real ksecretd (the classification's one environmental
  assumption); (b) optional cold-daemon variant — retry the Missing arm only when the file fallback is
  also absent.
- **Ticket 13 (apt repo, GH #45) MERGED** — PR #68 (main 4a232c6) after 5 rounds (Sol r1 high 18 →
  Opus fix → Sol re-r2 med 7 → Opus fix → Opus r3 high 2 minor, discard rule fired → driver inline →
  Sol re-r4 med 1 minor → driver inline → Opus delta-verify + guard hoist, closed).
  **NEXT TICKET: 14 (release workflow + CI smoke, `.scratch/voisu-friends/issues/14-*.md`)** — on-tag
  pipeline: build deb on Ubuntu → publish gh-pages apt repo via packaging/apt/make-apt-repo.sh
  (real-key loopback signing, workflow concurrency group) → AUR deploy; COPR self-triggers. Open
  design points from #68: Ubuntu target/Overlay gtk4-layer-shell dep (24.04 unsupported — split
  Overlay deb?), key-rotation sub-ms window, Valid-Until refresh duty. Then ticket 16 (GH #65, ring
  license trees deb+rpm — MUST precede first tagged release), then 15 (live desktop validation, HITL).
- **Sol/cladex budget: 1 review remaining — RESERVED for ticket 14's first review** (Raja cap
  2026-07-20 + same-evening +1; spent on #68 re-r2 and re-r4). At 0 or cladex death → ALL reviews to
  Opus 4.8 subagent high until Raja re-enables codex. See CLAUDE.md routing.
- **Quality-gate transcript drops are Raja's dominant felt failure** (~11 in 4 days; by design — the
  reconciliation divergence gate refuses unsafe delivery). Product/tuning question (degraded
  best-effort delivery?) — ASK RAJA before speccing anything.
- **HITL queue for Raja:** (a) **merge PR #70**; (b) **rebuild + upgrade installed RPM to current
  main after #70 merges** (installed 0.1.0-1.git58a607f is 77+ commits behind, misses PR #61
  Trigger-Key self-heal; one-time `dnf downgrade`/`--oldpackage` due to Release-scheme change);
  (c) **apt-repo one-time setup** (seed orphan gh-pages via worktree, enable Pages branch/root,
  fingerprint-verified smoke — checklist in packaging/apt/README.md); (d) COPR custom-source package
  config + webhook-rebuild flag (ticket 12 notes); (e) first AUR push (deploy key); (f) reboot+suspend
  Trigger-Key self-heal check (PR #61); (g) live KDE guarded-mode test (PR #56); (h) GNOME VM visual
  check (PR #59); (i) `voisu setup` smoke vs real ksecretd (PR #62) — can double as the
  empty-stderr-on-no-match verification above; (j) AUR TOTP when aurweb ships 2FA; (k) delete
  sandbox-validation.conf dropins on next RPM install.
- **Merging PRs is classifier-blocked for the agent** — Raja merges via `! gh pr merge <n> --merge --delete-branch`.

## Status
- **Phase B packaging: tickets 10 (deb PR #63), 11 (AUR PR #64), 12 (COPR PR #66), 13 (apt PR #68)
  ALL MERGED.** Phase A complete (01–08); ticket 09 accounts/keys live.
- **Apt channel (ticket 13):** GitHub Pages + apt-ftparchive self-hosted (Cloudsmith rejected —
  ticket 09 provisioned self-signing; zero third-party). `packaging/apt/make-apt-repo.sh` =
  transactional publisher (pool-wide deb validation, symlink-proof incl. recovery path,
  one-primary-key pin, published-bytes immutability, by-hash + Acquire-By-Hash, Valid-Until 30d,
  keep-3 retention, flock + stage/swap + crash-recovery restore-before-GC, restore-verified rollback);
  `apt-e2e.sh` = podman e2e (commit+image-ID-pinned manifest, ephemeral key, wrong-key negative test,
  Release.gpg fallback, --only-upgrade proof); `README.md` = friends snippet (fingerprint-pinned,
  exactly-one-primary-key, /etc/apt/keyrings) + maintainer guide + HITL checklist.
  **Support matrix: Ubuntu 26.04 amd64 only.** Real-key signing lands in CI (ticket 14).
- **Failure triage (2026-07-20, row 188):** 7 journal signatures → 3 stale pre-install (already fixed
  in running binary), overlay noise benign, PR #61 fix absent from installed RPM (upgrade), quality
  gate by design (product question), mid-session secret denial → GH #69 → PR #70.
- Test baseline: **431/0 on main; 439/0 on PR #70 branch** (8 new tests). CI green both.
- `docs/model-benchmark.md` rows through 189. Sol/Terra/Luna vs Opus final report was due after
  ticket 13 — now unblocked; confirm with Raja whether to wait for ticket 14 data.

## Architecture map
- Domain, IPC, Transcript decision, FocusProbe trait, ShortcutEvent, KeyDiagnosis, ProviderKeyStatus -> `crates/voisu-core/src/lib.rs`
- Fedora capture/provider/clipboard/portal/libei adapters, GuardedDelivery, SecretToolStore (+retry/cache PR #70) -> `crates/voisu-app/src/system.rs`
- Setup wizard -> `crates/voisu-app/src/setup.rs` · Fallback credentials file -> `crates/voisu-app/src/secret_file.rs`
- Focus probes -> `crates/voisu-app/src/focus.rs` · Daemon + shortcut self-heal -> `crates/voisu-app/src/bin/voisu-daemon.rs`
- Config writer -> `crates/voisu-app/src/config.rs` · Dictionary -> `crates/voisu-app/src/dictionary.rs`
- CLI -> `crates/voisu-app/src/bin/voisu.rs` · Overlay -> `bin/voisu-overlay.rs`, `overlay.rs`, `feedback.rs`
- Service lifecycle -> `crates/voisu-app/src/service.rs` · CI -> `.github/workflows/ci.yml`
- RPM -> `packaging/voisu.spec`, `build-rpm.sh` · deb -> `[package.metadata.deb]`, `packaging/build-deb.sh`, `packaging/deb/`
- AUR -> `packaging/aur/voisu{,-bin}/` · COPR -> `packaging/build-srpm.sh`, `packaging/copr/`, `.github/workflows/copr-trigger.yml`
- Apt repo -> `packaging/apt/{make-apt-repo.sh,apt-e2e.sh,README.md,voisu-archive-keyring.asc}`
- Friends map + tickets -> `.scratch/voisu-friends/` (map.md; issues/01–16; assets/)

## Stack & run
- Stack: Rust 2024 + Tokio + serde + zbus 5 + GTK4 (opt-in) + runtime libei + flacenc · Run: `cargo run -p voisu-app --bin voisu-daemon` · Test: `cargo test --workspace`
- Robust test count: `cargo test --workspace 2>&1 | grep -oE "[0-9]+ passed; [0-9]+ failed" | awk '{p+=$1; f+=$3} END {print p, f}'`

## Key decisions (top; full log in decisions.md)
- **Apt channel = GitHub Pages self-hosted, apt-ftparchive, own GPG key (ticket 13):** Cloudsmith
  rejected; published bytes immutable — any respin needs a version bump.
- **RPM Release policy (ticket 12):** pre-release `0.<count>.<ct>.git<sha>`, tagged plain `N` from
  committed packaging/rpm-release; one-time --oldpackage on Raja's box.
- **deb must be built on Ubuntu** (dpkg-shlibdeps ABI floors). **24.04 LTS lacks gtk4-layer-shell** →
  Overlay dep gates the install base; ticket 14 must pick the target (split Overlay deb?).
- **No `keyring` crate** — secret-tool boundary (PR #70 wraps the shell-out: retry + TTL cache;
  invalidation is TTL-only, curl --fail collapses 4xx so no auth-rejection hook).
- **Guarded delivery:** strict stable_id, fail closed.

## Gotchas
- **Check `git branch --show-current` before tree work** — tree may sit on a fix branch (PR #70 left
  it there); don't switch branches while a tree-using agent runs.
- **Driver docs commits leaked into PR #68's diff** (tree was on the feature branch) — commit driver
  docs to main via a TEMP WORKTREE (git worktree add from origin/main), never on feature branches.
- **cladex JSON carries ONLY the final message** — demand a self-contained final message; parse the
  output file line-wise (proxy banner precedes the JSON line).
- **Subagents stall on background waiters** — dispatch prompts must mandate foreground-only.
- **secret-tool lookup: genuine no-match = exit 1 + EMPTY stderr** — PR #70's classification relies on
  this; verify against real ksecretd (HITL item i).
- **`pw-record` is in `pipewire-audio`, `wpctl` in `wireplumber`** — base `pipewire` alone fails at first Recording.
- **ring fails to link under LTO** (Arch `!lto`); ring license tree must keep upstream paths (ticket 16).
- **COPR:** SRPM phase networked, mock phase NOT; names case-sensitive; plain `rpmbuild -bs` (bake %global).
- **rpmbuild %check needs TMPDIR=/var/tmp RUST_TEST_THREADS=4 exported IN the spec; run non-root.**
- No local clippy/shellcheck — CI is the oracle. gh GraphQL PR-edit broken — PATCH via `gh api`.
- `build-*.sh` need a clean COMMITTED checkout. Disk tight: `cargo clean` before RPM builds; remove
  pulled container images after packaging work.
- Use `CONTEXT.md` terms exactly; default builds GTK-free (`--features overlay` for the Overlay).
