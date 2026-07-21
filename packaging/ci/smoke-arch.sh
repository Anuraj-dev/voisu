#!/usr/bin/env bash
set -euo pipefail

# Arch install-smoke for the release gate (ticket 14).
#
# Builds and installs the voisu SOURCE PKGBUILD pointed at the current release
# tag via `makepkg -si`, then runs namcap on the PKGBUILD and the built package.
# Publishing is gated on this job, so it proves the source PKGBUILD builds a
# clean, installable package from the tag before the AUR `voisu` repo is updated.
#
# NON-ROOT: makepkg refuses to run as root, so the build runs as a `builder`
# user with passwordless sudo (pacman needs root to install build deps and the
# finished package).
#
# --nocheck: the PKGBUILD check() runs the workspace test suite, which is
# already gated on every push/PR by .github/workflows/ci.yml; re-running it here
# would only lengthen the release gate. This is the one deliberate degradation
# on the Arch path; see packaging/RELEASING.md.
#
# TAG POINTER: the committed PKGBUILD's pkgver still reads the previous release
# until the publish job bumps it, so the smoke rewrites pkgver to THIS tag before
# building (the source= URL then fetches the live tag tarball).
#
# Containers have no systemd user session, so unit files are checked statically
# with `systemd-analyze verify`.
#
# Usage: smoke-arch.sh <version>   (e.g. 0.1.0 for tag v0.1.0)

version=${1:?usage: smoke-arch.sh <version>}
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$script_dir/../.." && pwd)

echo "== provision Arch build toolchain =="
pacman -Syu --noconfirm --needed base-devel git namcap rust sudo systemd

useradd -m builder 2>/dev/null || true
echo 'builder ALL=(ALL) NOPASSWD: ALL' > /etc/sudoers.d/builder
chmod 440 /etc/sudoers.d/builder

# Stage the recipe + its local sidecar sources, pointed at THIS tag.
build=/home/builder/voisu-src
rm -rf "$build"; mkdir -p "$build"
cp "$repo_root"/packaging/aur/voisu/* "$build/"
sed -i "s/^pkgver=.*/pkgver=${version}/; s/^pkgrel=.*/pkgrel=1/" "$build/PKGBUILD"
chown -R builder:builder "$build"

echo "== makepkg -si (source build + install; --nocheck, see header) =="
su builder -c "cd '$build' && makepkg -si --noconfirm --nocheck --needed"

echo "== namcap (PKGBUILD + built package) =="
pkg=$(su builder -c "cd '$build' && ls voisu-*-x86_64.pkg.tar.zst" | head -1)
namcap "$build/PKGBUILD" | tee /tmp/namcap-pkgbuild.txt || true
namcap "$build/$pkg"     | tee /tmp/namcap-pkg.txt || true
# namcap emits WARNINGS (W:) for the subprocess/dlopen/D-Bus runtime deps its
# ELF scan cannot see (documented in the PKGBUILD); only ERRORS (E:) fail.
if grep -E '(^|[[:space:]])E: ' /tmp/namcap-pkgbuild.txt /tmp/namcap-pkg.txt; then
    echo "FAIL: namcap reported errors"; exit 1
fi

echo "== binaries =="
test -x /usr/bin/voisu
test -x /usr/bin/voisu-daemon
test -x /usr/bin/voisu-overlay
voisu --version
voisu-daemon --help >/dev/null

echo "== systemd-analyze verify (both user units) =="
systemd-analyze verify /usr/lib/systemd/user/voisu.service
systemd-analyze verify /usr/lib/systemd/user/voisu-overlay.service

echo "PASS: archlinux install-smoke (makepkg -si + namcap + binaries + unit verify)"
