#!/usr/bin/env bash
set -euo pipefail

# Publish the AUR packages on release (ticket 14).
#
#   voisu (source):     bump pkgver to the tag, pin the GitHub tag-archive
#                       sha256 with updpkgsums, regenerate .SRCINFO, push.
#   voisu-bin (binary): bump pkgver, pin sha256sums against the release tarball
#                       (identical bytes are supplied locally so we do not race
#                       the GitHub Release upload), regenerate .SRCINFO, push.
#
# Both AUR git repos are pushed over SSH. The caller must have already installed
# the AUR deploy key and a verified known_hosts entry for aur.archlinux.org, and
# must run this as a NON-ROOT user (makepkg/updpkgsums refuse root).
#
# Requires: makepkg, updpkgsums (pacman-contrib), git, ssh.
#
# Usage: aur-publish.sh <version> <repo_root> <bin_tarball>

version=${1:?version}
repo_root=${2:?repo_root}
bin_tarball=${3:?bin_tarball}
test -r "$bin_tarball"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# Replace the tracked files of an AUR repo with a freshly-staged package dir,
# then commit + push if anything changed. .git is preserved.
aur_push() {
    local name=$1 dir=$2
    git clone "ssh://aur@aur.archlinux.org/${name}.git" "$work/$name.git"
    find "$work/$name.git" -maxdepth 1 -mindepth 1 ! -name .git -exec rm -rf {} +
    cp -a "$dir"/. "$work/$name.git/"
    (
        cd "$work/$name.git"
        git add -A
        if git diff --cached --quiet; then
            echo "$name: no changes to push"
            exit 0
        fi
        git -c user.name='Voisu Release' -c user.email='rajasaikia1644@gmail.com' \
            commit -m "upgpkg: ${name} ${version}-1"
        git push origin master
    )
}

# --- voisu (source PKGBUILD) ---
echo "== stage + push AUR voisu (source) =="
src="$work/voisu"; mkdir -p "$src"
cp -a "$repo_root"/packaging/aur/voisu/. "$src/"
sed -i "s/^pkgver=.*/pkgver=${version}/; s/^pkgrel=.*/pkgrel=1/" "$src/PKGBUILD"
# updpkgsums downloads the (already-live) tag archive and pins its sha256,
# leaving the ring sidecar sums intact.
( cd "$src" && updpkgsums )
( cd "$src" && makepkg --printsrcinfo > .SRCINFO )
aur_push voisu "$src"

# --- voisu-bin (prebuilt PKGBUILD) ---
echo "== stage + push AUR voisu-bin (prebuilt) =="
bin="$work/voisu-bin"; mkdir -p "$bin"
cp -a "$repo_root"/packaging/aur/voisu-bin/. "$bin/"
sed -i "s/^pkgver=.*/pkgver=${version}/; s/^pkgrel=.*/pkgrel=1/" "$bin/PKGBUILD"
# The release asset bytes == the tarball this workflow built, so pin the sha256
# locally rather than downloading the just-published asset (avoids a race with
# the GitHub Release job). makepkg validates the same bytes at the user's end.
sum=$(sha256sum "$bin_tarball" | awk '{print $1}')
sed -i "s/^sha256sums=.*/sha256sums=('${sum}')/" "$bin/PKGBUILD"
( cd "$bin" && makepkg --printsrcinfo > .SRCINFO )
aur_push voisu-bin "$bin"

echo "AUR publish complete for ${version}"
