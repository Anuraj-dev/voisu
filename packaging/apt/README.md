# Voisu apt repository

Voisu ships a self-hosted, GPG-signed apt repository so friends can install
Voisu once and get upgrades through `apt` like any other package. The repository
is served as static files from GitHub Pages:

- Base URL: `https://anuraj-dev.github.io/voisu`
- Suite / component / arch: `stable` / `main` / `amd64`
- Signing key fingerprint: `4149EE3868B36B6007592966D08BCFDC34125B28`
  (ed25519, uid `Voisu Package Signing <rajasaikia1644@gmail.com>`)

## Supported platform (exact)

| Component | Support |
|-----------|---------|
| Distro    | **Ubuntu 26.04 LTS (`resolute`)**, amd64 |
| Older Ubuntu | Not supported — see below |

Voisu's `.deb` is built on Ubuntu 26.04 LTS and its dependency floors (glibc,
GTK4) are encoded from that release, so **it is not installable on 24.04 LTS or
earlier**. In particular the Overlay links `gtk4-layer-shell`, which 24.04 does
not package at all. The build/test image is pinned to `ubuntu:26.04` (a specific
release, not the moving `rolling` tag) precisely so the floors don't drift
silently. If you are on an older Ubuntu, Voisu is not available for you yet
through this channel.

---

## Install (for friends)

Copy–paste this block. It installs the prerequisites, downloads the signing key,
**verifies its fingerprint against the value pinned above** before trusting it
(so a compromised Pages host can't swap in a different key), installs it as a
dedicated keyring, pins the repo to that one key with `signed-by=`, and installs
Voisu. Signatures are verified the whole way — no `--allow-unauthenticated`, no
`trusted=yes`.

```sh
set -eu
EXPECT=4149EE3868B36B6007592966D08BCFDC34125B28

# 1. Prerequisites.
sudo apt-get update
sudo apt-get install -y ca-certificates curl gnupg

# 2. Fetch the key to a temp file and verify it BEFORE trusting it. The bundle
#    must contain EXACTLY ONE primary key with the expected fingerprint — a
#    bundle with an extra appended primary key must be rejected (checking only
#    the first fingerprint would still trust the whole file you dearmor).
tmp="$(mktemp -d)"
curl -fsSL https://anuraj-dev.github.io/voisu/voisu-archive-keyring.asc -o "$tmp/key.asc"
npub="$(gpg --show-keys --with-colons "$tmp/key.asc" | grep -c '^pub:' || true)"
[ "$npub" = 1 ] || { echo "REJECT: bundle has $npub primary keys, expected 1" >&2; exit 1; }
got="$(gpg --show-keys --with-colons "$tmp/key.asc" | awk -F: '$1=="pub"{p=1;next} p&&$1=="fpr"{print $10;exit}')"
[ "$got" = "$EXPECT" ] || { echo "FINGERPRINT MISMATCH: got $got, expected $EXPECT" >&2; exit 1; }

# 3. Install the verified key as a dedicated keyring (0644, apt-sandbox readable).
sudo install -d -m 0755 /etc/apt/keyrings
gpg --dearmor < "$tmp/key.asc" > "$tmp/voisu.gpg"
sudo install -m 0644 "$tmp/voisu.gpg" /etc/apt/keyrings/voisu-archive-keyring.gpg
rm -rf "$tmp"

# 4. Add the repo, pinned to that key and to amd64 only.
echo 'deb [signed-by=/etc/apt/keyrings/voisu-archive-keyring.gpg arch=amd64] https://anuraj-dev.github.io/voisu stable main' \
  | sudo tee /etc/apt/sources.list.d/voisu.list

# 5. Update and install.
sudo apt-get update
sudo apt-get install -y voisu
```

Upgrades then come with the usual `sudo apt-get update && sudo apt-get upgrade`.

After install, enable Voisu for your user (it ships as systemd **user**
services and is intentionally not auto-started):

```sh
systemctl --user daemon-reload
systemctl --user enable --now voisu.service
# optional on-screen Overlay:
systemctl --user enable --now voisu-overlay.service
```

### Uninstall

Disable the user services first (a package remove runs as root and cannot touch
your per-user units), then remove the package and the repo:

```sh
systemctl --user disable --now voisu-overlay.service 2>/dev/null || true
systemctl --user disable --now voisu.service 2>/dev/null || true
systemctl --user daemon-reload
systemctl --user reset-failed 2>/dev/null || true

sudo apt-get remove -y voisu
sudo rm -f /etc/apt/sources.list.d/voisu.list /etc/apt/keyrings/voisu-archive-keyring.gpg
```

---

## Maintainer guide

### Repository layout (the gh-pages branch)

The `gh-pages` branch of this repo *is* the published tree. GitHub Pages serves
its root at the base URL above.

```
/                                   -> https://anuraj-dev.github.io/voisu/
├── .nojekyll                       # disables Jekyll so files serve verbatim
├── voisu-archive-keyring.asc       # public signing key (friends fingerprint-pin it)
├── pool/
│   └── main/v/voisu/
│       └── voisu_<version>_amd64.deb
└── dists/
    └── stable/
        ├── InRelease               # clearsigned Release
        ├── Release                 # Origin/Label/Suite/Codename/... + Valid-Until + checksums
        ├── Release.gpg             # detached signature over Release
        └── main/binary-amd64/
            ├── Packages
            ├── Packages.gz
            └── by-hash/SHA256/<digest>   # content-addressed index copies
```

### Publishing a build

`packaging/apt/make-apt-repo.sh` does everything: it drops the `.deb`(s) into the
pool, regenerates `Packages`(+`.gz`) with `apt-ftparchive`, writes a `Release`
(with `Valid-Until` and `Acquire-By-Hash: yes`), and produces both `InRelease`
and `Release.gpg`. It is **idempotent** and enforces several invariants:

- **Immutable versions.** Re-publishing a version with different bytes is a hard
  error; identical bytes are a no-op.
- **Signing-boundary validation.** Every input `.deb` must be a real Voisu amd64
  package with a sane Debian version and a canonical filename.
- **Retention.** The pool keeps the newest `VOISU_APT_KEEP` versions (default 3 =
  current + 2 prior); older, now-unreferenced `.debs` are deleted before metadata
  is regenerated, bounding gh-pages storage.
- **Atomic metadata.** The full metadata set is staged in a sibling dir,
  self-tested (exact SHA256 block + signature verification), and swapped in under
  an exclusive `flock`, so a directly-served checkout never exposes half-written
  or mis-signed metadata.
- **Freshness.** `Release` carries `Valid-Until` = now + `VOISU_APT_VALID_DAYS`
  (default 30). **This means the repo MUST be re-published (re-signed) at least
  every 30 days or `apt update` starts rejecting it as expired.** Ticket 14 must
  add a scheduled workflow that re-runs this script to refresh the signature even
  when there is no new build. Recovery from an expired repo is simply to
  re-publish (any run re-stamps `Valid-Until`).

```sh
# Build the .deb (must run on Ubuntu 26.04 — see packaging/build-deb.sh):
packaging/build-deb.sh

# Publish it into a checkout/worktree of the gh-pages branch:
VOISU_APT_GPG_KEY=4149EE3868B36B6007592966D08BCFDC34125B28 \
  packaging/apt/make-apt-repo.sh /path/to/gh-pages-worktree dist/deb/voisu_*.deb
```

Then commit and push the gh-pages worktree. In CI (ticket 14) the private key is
imported into an ephemeral `GNUPGHOME` and `VOISU_APT_GPG_PASSPHRASE_FILE` points
at a passphrase file so gpg can sign non-interactively via loopback pinentry. The
script never prints or writes the private key or passphrase. **Ticket 14 must
also put the publish job in a GitHub Actions `concurrency` group** so two release
jobs never race the gh-pages branch (the script's `flock` only serializes runs on
one machine).

Requirements on the publishing host: `apt-utils` (`apt-ftparchive`), `gnupg`,
`dpkg-dev` (`dpkg-deb`), `coreutils`, `util-linux` (`flock`), `gzip`.

### End-to-end verification

`packaging/apt/apt-e2e.sh` builds a real `.deb` and a strictly-newer one on
`ubuntu:26.04`, publishes with an ephemeral key, serves over local HTTP, adds the
repo the documented way, proves a wrong key is rejected, proves the detached
`Release.gpg` path also authenticates (InRelease removed), installs, republishes,
and asserts `apt upgrade` picks up the newer version. It requires a clean
checkout and derives every script under test from the committed `HEAD` (not the
live tree), recording a manifest so the phased runs can't mix commits.

### One-time setup (HITL — Raja does these once)

The publish script never touches repo settings or pushes branches. These steps
are manual and only need doing once. **Do them in a separate worktree so your
main checkout is never destructively wiped.**

- [ ] **Create and seed the `gh-pages` branch in a worktree** (keeps `main`
      intact):
      ```sh
      # from your normal checkout:
      git worktree add --orphan -b gh-pages ../voisu-ghpages
      cd ../voisu-ghpages
      touch .nojekyll
      git add .nojekyll && git commit -m "chore: seed gh-pages apt repo branch"
      git push -u origin gh-pages
      cd -            # back to your main checkout; the worktree stays for publishing
      ```
      (On older git without `worktree add --orphan`: `git worktree add --detach
      ../voisu-ghpages` then `git switch --orphan gh-pages` inside it.)
- [ ] **Enable GitHub Pages.** Repo → Settings → Pages → Source: *Deploy from a
      branch* → Branch: `gh-pages`, folder `/ (root)` → Save. Confirm the site
      publishes at `https://anuraj-dev.github.io/voisu/`.
- [ ] **Confirm the signing-key secrets exist** (from ticket 09): repo secrets
      `GPG_PRIVATE_KEY` and `GPG_PASSPHRASE`. Ticket 14 wires the release
      workflow that imports them and calls `make-apt-repo.sh`.
- [ ] **Post-deploy smoke test** (once Pages is live). This asserts the served
      key's fingerprint, verifies the signed metadata, does an *isolated* apt
      update against only the Voisu source, and actually resolves + fetches the
      package — not just "a key block came back":
      ```sh
      set -eu
      BASE=https://anuraj-dev.github.io/voisu
      EXPECT=4149EE3868B36B6007592966D08BCFDC34125B28
      tmp="$(mktemp -d)"; export GNUPGHOME="$tmp/gnupg"; mkdir -m700 "$GNUPGHOME"
      # Isolate ALL apt state (lists, cache, keyring) to $tmp so update/policy/
      # download can only be satisfied by the Voisu source below — never by a
      # pre-configured mirror — and the whole test runs UNPRIVILEGED: no sudo,
      # nothing under /etc or /var is touched or left behind.
      SRC="$tmp/voisu.list"
      mkdir -p "$tmp/state/lists/partial" "$tmp/cache"
      APT="-o Dir::Etc::sourcelist=$SRC -o Dir::Etc::sourceparts=/dev/null \
           -o Dir::State=$tmp/state -o Dir::Cache=$tmp/cache \
           -o APT::Get::List-Cleanup=0"

      # 1. served bundle: exactly one primary key, fingerprint == pinned one
      curl -fsSL "$BASE/voisu-archive-keyring.asc" -o "$tmp/key.asc"
      npub="$(gpg --show-keys --with-colons "$tmp/key.asc" | grep -c '^pub:' || true)"
      [ "$npub" = 1 ] || { echo "REJECT: $npub primary keys"; exit 1; }
      got="$(gpg --show-keys --with-colons "$tmp/key.asc" | awk -F: '$1=="pub"{p=1;next} p&&$1=="fpr"{print $10;exit}')"
      [ "$got" = "$EXPECT" ] || { echo "MISMATCH: $got"; exit 1; }

      # 2. the signed InRelease verifies under that key
      gpg --dearmor < "$tmp/key.asc" > "$tmp/voisu.gpg"
      curl -fsSL "$BASE/dists/stable/InRelease" -o "$tmp/InRelease"
      gpgv --keyring "$tmp/voisu.gpg" "$tmp/InRelease" && echo "InRelease OK"

      # 3. isolated update + ASSERTED policy + identity-checked fetch
      #    (signed-by points at the verified TEMP keyring — the smoke test never
      #    installs anything into the system's /etc/apt/keyrings)
      echo "deb [signed-by=$tmp/voisu.gpg arch=amd64] $BASE stable main" > "$SRC"
      apt-get update $APT
      # candidate must come from OUR base URL, not some other configured source
      apt-cache policy voisu $APT | tee "$tmp/policy.txt"
      grep -q "$BASE" "$tmp/policy.txt" || { echo "FAIL: voisu candidate not from $BASE"; exit 1; }
      # fetch from the isolated source and verify the downloaded file's identity
      ( cd "$tmp" && apt-get download voisu $APT )
      deb="$(ls "$tmp"/voisu_*_amd64.deb)"
      [ "$(dpkg-deb --field "$deb" Package)" = voisu ] \
        && [ "$(dpkg-deb --field "$deb" Architecture)" = amd64 ] \
        && echo "package fetch OK: $(basename "$deb")"
      rm -rf "$tmp"
      ```
