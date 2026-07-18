# On-tag release workflow + container install-smoke gates

**Label:** `wayfinder:task` (AFK, implementation)
**Status:** open
**Blocked by:** 10-cargo-deb-package, 11-aur-packages, 12-copr-channel, 13-apt-repo-channel
**Blocks:** 15-live-desktop-validation

## Question

One GitHub Actions workflow on tag push: build release binaries → cargo-deb →
publish apt repo artifacts → run the AUR deploy action (bump voisu-bin, and
voisu source pkgver) → attach artifacts to the GitHub Release; COPR rebuilds
itself via webhook. Then the smoke gate (answers Raja's "can CI test the
distros?" — the part CI CAN do): matrix of fedora:latest / ubuntu:24.10 /
archlinux containers that install the fresh package from its real channel,
assert binaries run (`voisu --version`, daemon --help), `systemd-analyze
verify` both units, lintian/namcap clean. Release publishing blocks on the
smoke gate passing. Document the runbook in packaging docs (NOT
STATE/benchmark). Routing: Terra high (CI orchestration with real failure
modes), Sol review.
