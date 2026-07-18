# .deb package via cargo-deb

**Label:** `wayfinder:task` (AFK, implementation)
**Status:** open
**Blocked by:** 09-packaging-accounts-setup
**Blocks:** 13-apt-repo-channel, 14-release-workflow-ci-smoke

## Question

`[package.metadata.deb]` config: binaries, user units shipped as plain assets
to `/usr/lib/systemd/user/`, GTK/portal/PipeWire/libei runtime deps by exact
Ubuntu package names (research digest §3 / packaging scout report), custom
postinst that PRINTS `systemctl --user enable --now` instructions (mirror the
RPM's UX — do not silently enable), postrm counterpart. Overlay feature-gated
build matching the RPM. Local verification: build the .deb, install in an
ubuntu:24.10 container, `systemd-analyze verify` the units, `lintian` clean or
justified overrides. Routing: Luna medium (packaging/config), Sol review.
