# Releasing Voisu

This is the operator runbook for cutting a Voisu release. One `v<major>.<minor>.<patch>`
tag push drives the whole pipeline: build → install-smoke gate → publish to every
channel. It is defined by `.github/workflows/release.yml` (plus the weekly
`.github/workflows/apt-refresh.yml`). Read this before your first tag; the rest of
the packaging docs cover the individual channels:

- apt repo internals + friend install + HITL setup: `packaging/apt/README.md`
- COPR self-trigger: `.github/workflows/copr-trigger.yml`
- deb / rpm / srpm builders: `packaging/build-deb.sh`, `packaging/build-rpm.sh`, `packaging/build-srpm.sh`

## TL;DR — cut a release

```sh
# 1. Land everything on main. The tree version is ALREADY correct — it is
#    auto-bumped per merged PR (see "Automatic version bumping" below), so the
#    voisu-app crate version is whatever the last release-worthy merge set it to.
#    The tag MUST match that version: tag v0.1.0 <-> version 0.1.0.
#    Releases come ONLY from main: validate fails closed if the tagged commit is
#    not an ancestor of origin/main.
# 2. Tag the exact release commit ON MAIN with the tree's current version and push.
git checkout main && git pull
git tag "v$(grep -m1 -E '^version = ' crates/voisu-app/Cargo.toml | cut -d'"' -f2)"
git push origin "v$(grep -m1 -E '^version = ' crates/voisu-app/Cargo.toml | cut -d'"' -f2)"
```

That is it. The push triggers `release.yml`. Watch the Actions run; nothing publishes
until the smoke gate is green.

## Automatic version bumping

You no longer bump the version by hand. `.github/workflows/version-bump.yml` runs on
every push to `main`: it reads the Conventional Commits merged since the last bump and,
if any are release-worthy, bumps the version (`feat:` → minor, `fix:`/`perf:` → patch,
a `!`/`BREAKING CHANGE:` → major) across all four synchronized places — both crate
`Cargo.toml`s, `packaging/voisu.spec` `Version:` + `%changelog`, and `Cargo.lock` — then
commits `chore(release): bump version to X.Y.Z` back to `main`. **Docs-only merges
(only `docs:`/`chore:`/`test:`/`ci:`/`style:`/`refactor:`) do not bump.** The tree is
therefore always tag-ready; cutting a release is just the tag push above. Any manual
version edit is pointless — it will be overwritten by the next auto-bump. To force a
bump without a merge (e.g. to promote a pending patch to a minor), run the workflow from
the Actions UI with the `bump` input. The workflow never tags and never publishes —
releasing stays the deliberate manual tag push.

## Pipeline stages

```
push tag vX.Y.Z
      │
      ▼
  validate ─ strict ^v[0-9]+\.[0-9]+\.[0-9]+$; FAILS the run on a malformed tag
      │
      ▼
   build ─── ubuntu:26.04 container: build-deb.sh (release .deb, revision -1),
      │      release binaries, and the AUR voisu-bin tarball + SHA256SUMS.
      │      Uploaded as the `release-artifacts` artifact.
      ▼
   smoke ─── matrix {ubuntu:26.04, fedora:latest, archlinux}, installs the
      │      artifact through each distro's REAL package path. PUBLISHING BLOCKS
      │      ON THIS JOB (see "Smoke gate" below).
      ▼
  publish (only after all smoke legs pass, in parallel except AUR):
     ├─ publish-apt      make-apt-repo.sh (real key, loopback) → push gh-pages
     ├─ publish-release  gh release create vX.Y.Z + attach deb, tarball, SHA256SUMS
     └─ publish-aur      (after publish-release) bump + push AUR voisu & voisu-bin
```

COPR is not in this workflow: its Custom-source webhook self-triggers on the same
tag via `copr-trigger.yml`, self-pins to the highest `v*` tag, and rebuilds from
`packaging/copr/make-srpm.sh`. Nothing to do.

## Smoke gate — what each leg proves (and what it can't)

The gate installs the freshly built artifacts through each distro's genuine
package manager. Because publishing is gated on this job, the live channels do not
exist yet, so each leg builds/serves the artifact locally instead of pulling the
not-yet-published channel.

| Leg | Script | Path exercised | Asserts |
|-----|--------|----------------|---------|
| ubuntu:26.04 | `packaging/ci/smoke-ubuntu.sh` | publishes the built `.deb` into a local apt repo signed with an **ephemeral** key, serves it over local HTTP, adds the repo the documented friend way (fingerprint-pin → `signed-by`), `apt install voisu` with signatures enforced | `voisu --version`, `voisu-daemon --help`, `systemd-analyze verify` on both user units, `lintian` clean-enough |
| fedora:latest | `packaging/ci/smoke-fedora.sh` | `build-rpm.sh` (non-root) builds the RPM, `dnf install` the main + Overlay subpackage | binaries run, `systemd-analyze verify` both units, `rpm --requires`/file-list checks |
| archlinux (arch) | `packaging/ci/smoke-arch.sh` | `makepkg -si` the **source** PKGBUILD pointed at the tag | `namcap` clean (errors only), binaries run, `systemd-analyze verify` both units |
| archlinux (arch-bin) | `packaging/ci/smoke-arch-bin.sh` | stages voisu-bin exactly as `aur-publish.sh` will (pkgver/sha256/.SRCINFO) but sources the **locally built release tarball**, then `makepkg -si` | proves the tarball layout matches the PKGBUILD's install steps; `namcap` clean, binaries run, `systemd-analyze verify` both units |

The ubuntu leg doubles as an end-to-end test of `make-apt-repo.sh` (including a
`--refresh` regression: it re-signs the staged repo and asserts the pool bytes are
unchanged and apt still installs). The arch-bin leg is what makes the **exact
artifact** shipped to the GitHub Release + AUR fail *here* if the tarball layout is
wrong, rather than in the first AUR user's terminal. voisu and voisu-bin declare a
mutual conflict, so the two arch legs run in separate containers.

The release tarball is **flat** — `voisu`, `voisu-daemon`, `voisu-overlay`, the two
units, `LICENSE` and `ring/` sit at the archive root — because voisu-bin's PKGBUILD
installs them directly from `$srcdir` with no versioned subdir.

### Deliberate degradations (accepted, documented here)

- **No systemd user session in containers.** None of the legs can enable/start the
  user services; that is the ticket 15 live-desktop smoke. Unit-file correctness is
  checked statically with `systemd-analyze verify`, which needs no session. This is
  why the fedora leg does **not** run the full `packaging/fedora-smoke.sh`
  `voisu service install` flow (it calls `systemctl --user daemon-reload`, which
  needs a session); it reuses only that harness's session-free assertions.
- **fedora is the long-pole job.** `build-rpm.sh` runs the workspace test suite, and
  rpmbuild's `%check` runs it again — there is no built-in `%check` switch to pass
  through, so the redundant run is accepted rather than engineered around. Run as a
  non-root `builder` with `TMPDIR=/var/tmp RUST_TEST_THREADS=4` (the tests spawn
  dbus-daemon/python/curl and a size-capped `/tmp` tmpfs would overflow). If this
  ever dominates release time, add an opt-in skip to `build-rpm.sh` and set it here.
- **arch skips `check()` (`makepkg --nocheck`).** The PKGBUILD `check()` is the same
  workspace test suite already gated on every push/PR by `ci.yml`; re-running 431
  tests in the release gate buys nothing. This is the one leg that skips tests, via
  a first-class makepkg switch.
- **lintian suppressions.** Three tags are suppressed, matching
  `packaging/deb/lintian-overrides`: `maintainer-script-calls-systemctl` (false
  positive — the scripts only print guidance), `no-manual-page` (built-in `--help`),
  `initial-upload-closes-no-bugs` (self-hosted repo, not the Debian archive). Any
  other error/warning fails the leg. If the first real tag surfaces a new tag,
  triage it and either fix the package or add a documented suppression.
- **namcap warnings allowed.** namcap's ELF scan cannot see the subprocess/dlopen/
  D-Bus runtime deps (pw-record, wpctl, wl-copy, secret-tool, portals — documented
  in the PKGBUILD), so it warns about them. Only `E:` errors fail; `W:` are expected.

## Secret inventory

| Secret | Used by | Purpose | Status |
|--------|---------|---------|--------|
| `GPG_PRIVATE_KEY` | publish-apt, apt-refresh | armored private signing key (fpr `4149EE38…34125B28`), imported into an ephemeral `GNUPGHOME` | provisioned in ticket 09 (confirm it exists) |
| `GPG_PASSPHRASE` | publish-apt, apt-refresh | passphrase for loopback pinentry (written to a temp file, never argv/log) | provisioned in ticket 09 |
| `COPR_WEBHOOK_URL` | copr-trigger.yml (not this workflow) | COPR Custom-source rebuild webhook | provisioned in ticket 12 |
| `AUR_SSH_PRIVATE_KEY` | publish-aur | SSH deploy key registered on the AUR account for the `voisu` and `voisu-bin` repos | **MUST be created by Raja** (see below) |

`GITHUB_TOKEN` (built-in) covers the gh-pages push and `gh release create`; the
publish jobs request `contents: write` for it.

### Creating `AUR_SSH_PRIVATE_KEY`

```sh
ssh-keygen -t ed25519 -C 'voisu-aur-deploy' -f voisu-aur-deploy -N ''
# Add voisu-aur-deploy.pub to your AUR account (https://aur.archlinux.org → My Account → SSH Public Key).
# Ensure the AUR `voisu` and `voisu-bin` package repos exist and are owned/co-maintained by that account.
gh secret set AUR_SSH_PRIVATE_KEY < voisu-aur-deploy   # the PRIVATE key
shred -u voisu-aur-deploy voisu-aur-deploy.pub
```

The workflow pins AUR's host key by its published ed25519 fingerprint
(`SHA256:RFzBCUItH9LZS0cKB5UE6ceAYhBD5C8GeOBip8Z11+4`) and fails closed on a
mismatch. If AUR ever rotates its host key, update that constant in
`.github/workflows/release.yml` (`publish-aur` → "Configure the AUR SSH deploy key").

## One-time prerequisites (HITL)

Done once, out-of-band. Full detail in `packaging/apt/README.md` → "One-time setup":

1. Seed the orphan `gh-pages` branch (in a worktree) and enable GitHub Pages on it.
2. Confirm `GPG_PRIVATE_KEY` / `GPG_PASSPHRASE` exist.
3. Create `AUR_SSH_PRIVATE_KEY` and register its public half on AUR; ensure the
   `voisu` and `voisu-bin` AUR repos exist.
4. Post-deploy apt smoke once Pages is live (script in the apt README).

Until the gh-pages branch exists, `apt-refresh.yml` (and `publish-apt`) will fail —
that is expected before the first release.

## The apt Valid-Until refresh

`make-apt-repo.sh` stamps the signed `Release` with `Valid-Until = now + 30 days`.
`apt-refresh.yml` runs `make-apt-repo.sh --refresh <gh-pages>` every Monday, which
re-indexes and re-signs the **existing** pool with a fresh `Valid-Until` and pushes
the metadata to gh-pages **without touching any published `.deb` bytes** (the
published-bytes-immutability invariant is preserved; only metadata is regenerated).
It shares the `voisu-gh-pages` concurrency group with `publish-apt`, so a release
publish and a scheduled refresh can never interleave on the branch.

Recovery from an expired repo is simply to re-run the refresh (Actions →
"Refresh apt repository signature" → Run workflow) or cut any release.

## Failure modes & respins

| Symptom | Cause | Fix |
|---------|-------|-----|
| `validate` fails immediately | tag is not strict `vX.Y.Z` | delete the bad tag, re-tag correctly |
| `validate` fails: "tag commit … is not on origin/main" | tag pushed on a commit that is not an ancestor of `main` (DRIVER DECISION: releases come only from main) | merge the work to main first, then tag the commit on main |
| `build` fails in build-deb.sh: "release build requires HEAD to be exactly at tag" | tag not on the release commit, or crate version ≠ tag | move the tag to the right commit / align `voisu-app` version with the tag |
| a `smoke` leg fails | a real packaging regression for that distro | fix the package; the gate did its job — nothing was published |
| `publish-apt` fails: "already published with DIFFERENT bytes" | re-tagging the same version with new content | published versions are immutable — bump the version (new tag) |
| `publish-aur` fails: "AUR … is at pkgver … newer than …; will not downgrade" | re-running an OLDER release's publish after a newer one already landed on AUR | expected guard — do nothing; the newer AUR package stands |
| gh-pages checkout fails | gh-pages branch missing | complete the HITL gh-pages setup |
| `publish-aur` host-key mismatch | AUR rotated its SSH host key | update the pinned fingerprint in `release.yml` |

### Respinning a release

- **Before any publish** (smoke red): fix the tree, delete and re-create the tag on
  the new commit, push again. Nothing to unwind.
- **A publish job failed after others succeeded**: the publish jobs are
  independent. Re-run only the failed job from the Actions UI. apt re-publish of the
  same bytes is idempotent; `gh release create` will refuse a duplicate tag (delete
  the release first if you must recreate it); AUR pushes are no-ops when unchanged and
  are guarded by `vercmp` against the remote pkgver (an equal version is a no-op, a
  newer remote hard-fails) so a re-run of an older release can never downgrade AUR.
  `publish-aur` also runs in a `voisu-aur-publish` concurrency group so two runs
  cannot interleave. **By design, an equal remote pkgver is a no-op even if content
  differs** (the pipeline always forces `pkgrel=1`), so a pkgrel-only / packaging-
  metadata-only AUR fix cannot ship on its own — cut a new patch-version release for
  it, per the respin policy.
- **Need genuinely new content under the same version number**: you cannot — apt and
  the published bytes are immutable per version. Bump the patch version and cut a new
  tag. (A deb-only rebuild of identical source can bump `VOISU_DEB_RELEASE` in the
  build job, but a fresh tag is the clean path.)

## Accepted risk — the signing-key rotation window

The apt install instructions (`packaging/apt/README.md`) have friends fetch the
public signing key from gh-pages and fingerprint-pin it before trusting it. During a
signing-key rotation there is a sub-millisecond window on the publisher where the
newly exported public key and the freshly signed metadata are swapped into place by
two separate renames; a client fetching in exactly that gap could see a new key with
old-key-signed metadata (or vice versa) and get a one-shot verification failure that
a retry resolves. We do not engineer around this: key rotation is rare and operator-
initiated, the failure is transient and safe (apt refuses rather than trusting the
wrong thing), and `apt update` retries. This is a documented, accepted risk, not a
bug to fix.
