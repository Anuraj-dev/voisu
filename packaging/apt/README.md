# Voisu apt repository (Ubuntu / Debian)

Voisu ships a self-hosted, GPG-signed apt repository so friends on Ubuntu (and
Debian derivatives) can install Voisu once and get upgrades through `apt` like
any other package. The repository is served as static files from GitHub Pages:

- Base URL: `https://anuraj-dev.github.io/voisu`
- Suite / component / arch: `stable` / `main` / `amd64`
- Signing key fingerprint: `4149EE3868B36B6007592966D08BCFDC34125B28`
  (ed25519, uid `Voisu Package Signing <rajasaikia1644@gmail.com>`)

---

## Install (for friends)

Copy–paste this block. It downloads the signing key, installs it as a
**dedicated** keyring (not the system-wide trust store), pins the repo to that
one key with `signed-by=`, and installs Voisu. Signatures are verified the whole
way — no `--allow-unauthenticated`, no `trusted=yes`.

```sh
# 1. Fetch the repo signing key and store it dearmored (binary) keyring.
sudo mkdir -p /usr/share/keyrings
curl -fsSL https://anuraj-dev.github.io/voisu/voisu-archive-keyring.asc \
  | sudo gpg --dearmor -o /usr/share/keyrings/voisu-archive-keyring.gpg

# 2. Add the repo, pinned to that key and to amd64 only.
echo 'deb [signed-by=/usr/share/keyrings/voisu-archive-keyring.gpg arch=amd64] https://anuraj-dev.github.io/voisu stable main' \
  | sudo tee /etc/apt/sources.list.d/voisu.list

# 3. Update and install.
sudo apt-get update
sudo apt-get install voisu
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

```sh
sudo apt-get remove voisu
sudo rm -f /etc/apt/sources.list.d/voisu.list /usr/share/keyrings/voisu-archive-keyring.gpg
```

---

## Maintainer guide

### Repository layout (the gh-pages branch)

The `gh-pages` branch of this repo *is* the published tree. GitHub Pages serves
its root at the base URL above.

```
/                                   -> https://anuraj-dev.github.io/voisu/
├── .nojekyll                       # disables Jekyll so files serve verbatim
├── voisu-archive-keyring.asc       # public signing key (friends fetch this)
├── pool/
│   └── main/v/voisu/
│       └── voisu_<version>_amd64.deb
└── dists/
    └── stable/
        ├── InRelease               # clearsigned Release
        ├── Release                 # Origin/Label/Suite/Codename/... + checksums
        ├── Release.gpg             # detached signature over Release
        └── main/binary-amd64/
            ├── Packages
            └── Packages.gz
```

### Publishing a build

`packaging/apt/make-apt-repo.sh` does everything: it drops the `.deb`(s) into
the pool, regenerates `Packages`(+`.gz`) with `apt-ftparchive`, writes a
`Release` with proper checksums, and produces both `InRelease` and
`Release.gpg`. It is **idempotent** — re-running keeps every previously
published `.deb` and just regenerates and re-signs the indices, so a newer
version is added alongside the old ones and `apt upgrade` picks it up.

```sh
# Build the .deb (must run on Ubuntu — see packaging/build-deb.sh):
packaging/build-deb.sh

# Publish it into a checkout of the gh-pages branch:
VOISU_APT_GPG_KEY=4149EE3868B36B6007592966D08BCFDC34125B28 \
  packaging/apt/make-apt-repo.sh /path/to/gh-pages-checkout dist/deb/voisu_*.deb
```

Then commit and push the gh-pages checkout. In CI (ticket 14) the private key is
imported into an ephemeral `GNUPGHOME` and `VOISU_APT_GPG_PASSPHRASE_FILE`
points at a passphrase file so gpg can sign non-interactively via loopback
pinentry. The script never prints or writes the private key or passphrase.

Requirements on the publishing host: `apt-utils` (`apt-ftparchive`), `gnupg`,
`dpkg-dev` (`dpkg-deb`), `gzip`.

### One-time setup (HITL — Raja does these once)

The publish script never touches repo settings or pushes branches. These steps
are manual and only need doing once:

- [ ] **Create and seed the `gh-pages` branch.** From a clean checkout:
      ```sh
      git switch --orphan gh-pages
      git rm -rf . 2>/dev/null || true
      # seed a first repo so the branch is non-empty (optional but tidy):
      #   run make-apt-repo.sh against this working tree, or just:
      touch .nojekyll && git add .nojekyll
      git commit -m "chore: seed gh-pages apt repo branch"
      git push -u origin gh-pages
      git switch main
      ```
- [ ] **Enable GitHub Pages.** Repo → Settings → Pages → Source: *Deploy from a
      branch* → Branch: `gh-pages`, folder `/ (root)` → Save. Confirm the site
      publishes at `https://anuraj-dev.github.io/voisu/`.
- [ ] **Confirm the signing-key secrets exist** (from ticket 09): repo secrets
      `GPG_PRIVATE_KEY` and `GPG_PASSPHRASE`. Ticket 14 wires the release
      workflow that imports them and calls `make-apt-repo.sh`.
- [ ] **Smoke-test the served key**: after Pages is live,
      `curl -fsSL https://anuraj-dev.github.io/voisu/voisu-archive-keyring.asc`
      should return the public key block.
