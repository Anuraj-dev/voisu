# AUR packages: voisu (source) + voisu-bin

**Label:** `wayfinder:task` (AFK implementation + HITL first push)
**Status:** open
**Blocked by:** 09-packaging-accounts-setup
**Blocks:** 14-release-workflow-ci-smoke

## Question

1. Source PKGBUILD `voisu`: `cargo build --release --locked` per ArchWiki Rust
   guidelines, `install -Dm` for binaries + user units to
   `/usr/lib/systemd/user/`, Arch dep names (gtk4, gtk4-layer-shell, pipewire,
   xdg-desktop-portal-kde/hyprland, libei), post_install echoing enable
   instructions (Arch convention: never silently enable).
2. `voisu-bin`: cargo-aur-generated from the GitHub release tarball, unit
   install lines added by hand.
3. `.SRCINFO` for both; `namcap` clean; test install in an archlinux container.
4. First AUR push is HITL (Raja's SSH key); subsequent pushes automated in
   ticket 14 via KSXGitHub/github-actions-deploy-aur.

Routing: Luna medium, Sol review.
