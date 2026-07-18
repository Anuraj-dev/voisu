# Self-hosted apt repo (GitHub Pages/aptly vs Cloudsmith)

**Label:** `wayfinder:task` (AFK, implementation; small HITL if Cloudsmith signup)
**Status:** open
**Blocked by:** 09-packaging-accounts-setup, 10-cargo-deb-package
**Blocks:** 14-release-workflow-ci-smoke

## Question

Stand up the Ubuntu update channel. First decide within the ticket: GitHub
Pages + aptly/apt-ftparchive (zero third-party, we own GPG + publishing script)
vs Cloudsmith/packagecloud free OSS tier (they handle signing/hosting; signup +
token needed). Then implement: repo structure, `InRelease`/`Release.gpg` signed
with ticket 09's key (if self-hosted), a one-line install snippet for friends
(`signed-by` keyring + sources.list entry), verified end-to-end in an
ubuntu container (add repo → apt install voisu → apt upgrade picks up a
re-published version). Skip Launchpad PPA (decided). Routing: Luna medium, Sol
review.
