#!/usr/bin/env bash
set -euo pipefail

# Arch PREBUILT (voisu-bin) install-smoke for the release gate (ticket 14).
#
# This is the leg that actually exercises the release tarball and the voisu-bin
# PKGBUILD contract BEFORE it is published to the GitHub Release + AUR: it stages
# the SAME pkgver / sha256sums / .SRCINFO that packaging/ci/aur-publish.sh will
# generate, but points the source at the LOCALLY built tarball, then runs
# `makepkg -si` and asserts the binaries/units/namcap. If the tarball layout does
# not match the PKGBUILD's install steps (e.g. a nested versioned dir), makepkg's
# package() fails here instead of in the first AUR user's terminal.
#
# voisu (source) and voisu-bin declare a mutual conflict, so this runs in its own
# container (a separate `arch-bin` matrix leg), never alongside the `arch` leg.
#
# NON-ROOT: makepkg refuses root, so it runs as a `builder` user with passwordless
# sudo (pacman needs root to install deps + the package). Containers have no
# systemd user session, so unit files are checked statically with
# `systemd-analyze verify`.
#
# Usage: smoke-arch-bin.sh <version> <tarball>

version=${1:?usage: smoke-arch-bin.sh <version> <tarball>}
tarball=${2:?usage: smoke-arch-bin.sh <version> <tarball>}
test -r "$tarball"
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$script_dir/../.." && pwd)

echo "== provision Arch toolchain =="
pacman -Syu --noconfirm --needed base-devel git namcap sudo systemd

useradd -m builder 2>/dev/null || true
echo 'builder ALL=(ALL) NOPASSWD: ALL' > /etc/sudoers.d/builder
chmod 440 /etc/sudoers.d/builder

# Stage the voisu-bin recipe exactly as aur-publish.sh will, but source the
# LOCAL tarball (a file source, resolved from the build dir) instead of the
# not-yet-published GitHub Release asset.
build=/home/builder/voisu-bin
rm -rf "$build"; mkdir -p "$build"
cp "$repo_root"/packaging/aur/voisu-bin/* "$build/"
cp "$tarball" "$build/voisu-${version}-x86_64.tar.gz"
sed -i "s/^pkgver=.*/pkgver=${version}/; s/^pkgrel=.*/pkgrel=1/" "$build/PKGBUILD"

# Before rewriting source= to the local file, VALIDATE the committed PKGBUILD's
# published GitHub-Release URL template: its basename (with pkgver + CARCH
# substituted) must equal the tarball this workflow actually built and will
# upload. A wrong path/arch token would otherwise pass the gate and then 404 for
# real AUR users installing voisu-bin.
tmpl=$(sed -n 's/^source=.*::\(.*\)")/\1/p' "$build/PKGBUILD" | head -1)
test -n "$tmpl" || { echo "FAIL: could not read the source URL template from voisu-bin PKGBUILD"; exit 1; }
expected_basename=$(CARCH=x86_64 pkgver="$version" bash -c "basename \"$tmpl\"")
if test "$expected_basename" != "voisu-${version}-x86_64.tar.gz"; then
    echo "FAIL: voisu-bin source URL basename '$expected_basename' != built tarball 'voisu-${version}-x86_64.tar.gz'" >&2
    exit 1
fi
echo "[evidence] published source URL basename resolves to the built tarball: $expected_basename"

# Repoint source at the local tarball (makepkg resolves a bare filename from the
# PKGBUILD dir) and pin its sha256 the same way aur-publish.sh does.
sum=$(sha256sum "$build/voisu-${version}-x86_64.tar.gz" | awk '{print $1}')
sed -i "s#^source=.*#source=(\"voisu-\${pkgver}-x86_64.tar.gz\")#" "$build/PKGBUILD"
sed -i "s/^sha256sums=.*/sha256sums=('${sum}')/" "$build/PKGBUILD"
chown -R builder:builder "$build"

echo "== .SRCINFO regenerates cleanly (as aur-publish.sh will emit) =="
su builder -c "cd '$build' && makepkg --printsrcinfo > .SRCINFO && head -3 .SRCINFO"

echo "== makepkg -si (prebuilt package from the release tarball) =="
su builder -c "cd '$build' && makepkg -si --noconfirm --needed"

echo "== namcap (PKGBUILD + built package) =="
pkg=$(su builder -c "cd '$build' && ls voisu-bin-*-x86_64.pkg.tar.zst" | head -1)
namcap "$build/PKGBUILD" | tee /tmp/namcap-pkgbuild.txt || true
namcap "$build/$pkg"     | tee /tmp/namcap-pkg.txt || true
if grep -E '(^|[[:space:]])E: ' /tmp/namcap-pkgbuild.txt /tmp/namcap-pkg.txt; then
    echo "FAIL: namcap reported errors"; exit 1
fi

echo "== binaries + units (from the prebuilt package) =="
test -x /usr/bin/voisu
test -x /usr/bin/voisu-daemon
test -x /usr/bin/voisu-overlay
voisu --version
voisu-daemon --help >/dev/null
systemd-analyze verify /usr/lib/systemd/user/voisu.service
systemd-analyze verify /usr/lib/systemd/user/voisu-overlay.service

echo "PASS: archlinux voisu-bin install-smoke (release tarball -> makepkg -si + namcap + binaries + unit verify)"
