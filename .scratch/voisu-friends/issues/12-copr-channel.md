# Fedora COPR channel (vendored crates + webhook auto-rebuild)

**Label:** `wayfinder:task` (AFK, implementation)
**Status:** open
**Blocked by:** 09-packaging-accounts-setup
**Blocks:** 14-release-workflow-ci-smoke

## Question

Adapt the existing `packaging/` RPM spec for COPR: builders have NO network, so
vendor crates (`cargo vendor` + `.cargo/config.toml` in the SRPM, or rust2rpm
approach — pick and justify), wire the COPR webhook from ticket 09 so tagged
pushes auto-rebuild, confirm COPR signs and `dnf copr enable <user>/voisu`
installs cleanly on a fedora container. Keep the local
`packaging/build-rpm.sh` path working (it's the dev-machine flow; COPR is the
friends channel). Mind the disk-space gotchas (STATE.md): vendoring adds bulk —
clean before builds. Routing: Luna medium (Terra high if the vendoring fight
gets ugly), Sol review.
