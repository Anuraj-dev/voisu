#!/usr/bin/env bash
set -euo pipefail

# Publish the Voisu self-hosted apt repository (ticket 13, GH issue #45).
#
# Voisu's Ubuntu update channel is a plain, GPG-signed apt repository served as
# static files off GitHub Pages (gh-pages branch of this repo, exposed at
# https://anuraj-dev.github.io/voisu/). No third-party package host: ticket 09
# already provisioned our own signing key, so we own signing AND hosting and
# carry zero extra accounts/tokens. This script is the whole publishing tool:
# given one or more .deb files and a repo working directory, it drops the debs
# into pool/, regenerates the binary-amd64 Packages index, writes a Release with
# proper checksums, and produces BOTH a clearsigned InRelease and a detached
# Release.gpg. It is idempotent: re-running against a populated repo keeps every
# previously published .deb and simply regenerates + re-signs the indices, so a
# newer version is added alongside the old ones and `apt upgrade` sees it.
#
# Index generation uses apt-ftparchive (from apt-utils), NOT dpkg-scanpackages:
# apt-ftparchive emits both the Packages index and, in one pass, the Release
# file with the MD5Sum/SHA256 blocks over every index -- exactly what a signed
# repo needs. dpkg-scanpackages only writes Packages and would leave us hand-
# rolling the Release checksums, which is more code, not less.
#
# SECRETS. The real signing key lives only as CI secrets (GPG_PRIVATE_KEY /
# GPG_PASSPHRASE) and in Raja's offline storage; this script never sees, prints,
# or writes a private key or passphrase. It signs with whatever secret key id is
# named by $VOISU_APT_GPG_KEY, using the gpg keyring already present in the
# environment ($GNUPGHOME). CI imports the private key into an ephemeral
# GNUPGHOME and points VOISU_APT_GPG_PASSPHRASE_FILE at a passphrase file for
# loopback pinentry; local verification uses a throwaway key with no passphrase.
# The PUBLIC key of the signing key is (re-)exported to the repo root as
# voisu-archive-keyring.asc so friends fetch the key from the same host that
# serves the packages, and so the served key always matches whatever key signed
# the indices (this keeps the end-to-end test with an ephemeral key coherent).

# --- usage -----------------------------------------------------------------
# VOISU_APT_GPG_KEY=<keyid> packaging/apt/make-apt-repo.sh <repo_dir> <deb>...
#
#   <repo_dir>  absolute path to the repo working directory (the gh-pages tree).
#   <deb>...    one or more .deb files to (re)publish into the pool.
#
# Environment:
#   VOISU_APT_GPG_KEY            signing key id/fingerprint
#                                (default: the Voisu package-signing fingerprint)
#   VOISU_APT_GPG_PASSPHRASE_FILE  optional path to a file holding the key
#                                passphrase; enables gpg loopback pinentry (CI).
#   VOISU_APT_ORIGIN / _LABEL / _SUITE / _CODENAME / _COMPONENT
#                                override the Release identity fields.

usage() {
    printf 'usage: VOISU_APT_GPG_KEY=<keyid> %s <repo_dir> <deb> [<deb>...]\n' \
        "$(basename "$0")" >&2
}

# The Voisu package-signing key (public fingerprint; safe to embed).
default_key='4149EE3868B36B6007592966D08BCFDC34125B28'

gpg_key=${VOISU_APT_GPG_KEY:-$default_key}
origin=${VOISU_APT_ORIGIN:-Voisu}
label=${VOISU_APT_LABEL:-Voisu}
suite=${VOISU_APT_SUITE:-stable}
codename=${VOISU_APT_CODENAME:-stable}
component=${VOISU_APT_COMPONENT:-main}
arch=amd64  # the only architecture cargo-deb builds for Voisu today

# --- argument parsing ------------------------------------------------------
if test "$#" -lt 2; then
    usage
    exit 1
fi
repo_dir=$1
shift
# Remaining args are .deb paths.
debs=("$@")

# --- tool checks (fail closed) ---------------------------------------------
for tool in apt-ftparchive gpg gzip dpkg-deb sha256sum; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        printf 'required tool %s not found; on Ubuntu/Debian: apt-get install apt-utils gnupg dpkg-dev\n' \
            "$tool" >&2
        exit 1
    fi
done

# --- repo_dir confinement (symlink-proof, no system dirs) ------------------
# The script does `rm -rf` inside <repo_dir> when regenerating indices, so the
# path must be a deliberate, dedicated directory. Require it absolute; reject
# any symlink traversal (a resolved path that differs from the purely lexical
# normalization means some component is a symlink and could redirect the rm);
# reject shallow/system locations. We create only the leaf directory, and only
# when its parent already exists, so a typo cannot spray a deep tree somewhere.
case "$repo_dir" in
    /*) : ;;
    *) printf 'refusing: repo dir %s must be an absolute path\n' "$repo_dir" >&2
       exit 1 ;;
esac
resolved=$(realpath -m "$repo_dir")
lexical=$(realpath -sm "$repo_dir")
if test "$resolved" != "$lexical"; then
    printf 'refusing to use %s: it traverses a symlink (resolves to %s); use a real, dedicated directory\n' \
        "$lexical" "$resolved" >&2
    exit 1
fi
repo_dir=$resolved
# Minimum depth 2 (e.g. /srv/voisu-apt), and never a well-known system root.
depth=$(printf '%s' "$repo_dir" | awk -F/ '{print NF-1}')
if test "$depth" -lt 2; then
    printf 'refusing to use %s: pick a dedicated directory at least two levels deep\n' "$repo_dir" >&2
    exit 1
fi
case "$repo_dir" in
    "$HOME" | /root | /usr | /usr/* | /etc | /etc/* | /var | /var/* | /bin | /bin/* | /boot | /boot/* | /lib | /lib/* | /sys | /sys/* | /proc | /proc/* | /dev | /dev/*)
        printf 'refusing to use %s: that is a system location, not a repo working directory\n' "$repo_dir" >&2
        exit 1 ;;
esac
parent=$(dirname "$repo_dir")
if ! test -d "$parent"; then
    printf 'refusing to use %s: its parent %s does not exist (create the parent deliberately first)\n' \
        "$repo_dir" "$parent" >&2
    exit 1
fi

# --- validate the .deb inputs ----------------------------------------------
for deb in "${debs[@]}"; do
    if ! test -f "$deb"; then
        printf 'refusing: %s is not a regular file\n' "$deb" >&2
        exit 1
    fi
    case "$deb" in
        *.deb) : ;;
        *) printf 'refusing: %s does not look like a .deb\n' "$deb" >&2
           exit 1 ;;
    esac
    # A structurally valid Debian archive: dpkg-deb reads the control member.
    if ! dpkg-deb --field "$deb" Package >/dev/null 2>&1; then
        printf 'refusing: %s is not a valid Debian package (dpkg-deb could not read it)\n' "$deb" >&2
        exit 1
    fi
done

# --- verify the signing key is usable (fail closed before touching files) --
if ! gpg --list-secret-keys "$gpg_key" >/dev/null 2>&1; then
    printf 'signing key %s not found in the gpg keyring ($GNUPGHOME); import it first\n' \
        "$gpg_key" >&2
    exit 1
fi

# gpg invocation prefix. --batch/--yes keep it non-interactive; a passphrase
# file switches on loopback pinentry (CI). Never expands the passphrase itself.
gpg_sign=(gpg --batch --yes --local-user "$gpg_key" --armor)
if test -n "${VOISU_APT_GPG_PASSPHRASE_FILE:-}"; then
    if ! test -f "$VOISU_APT_GPG_PASSPHRASE_FILE"; then
        printf 'VOISU_APT_GPG_PASSPHRASE_FILE points at %s which does not exist\n' \
            "$VOISU_APT_GPG_PASSPHRASE_FILE" >&2
        exit 1
    fi
    gpg_sign+=(--pinentry-mode loopback --passphrase-file "$VOISU_APT_GPG_PASSPHRASE_FILE")
fi

# --- lay out the repo tree -------------------------------------------------
# Debian pool convention: pool/<component>/<prefix>/<source>/ where <prefix> is
# the source name's first letter (or lib<x> -> libx). Voisu's source is
# "voisu", so the prefix is "v".
mkdir -p "$repo_dir"
pool_rel="pool/${component}/v/voisu"
dist_rel="dists/${suite}"
bindir_rel="${dist_rel}/${component}/binary-${arch}"
mkdir -p "$repo_dir/$pool_rel" "$repo_dir/$bindir_rel"

# Add each deb to the pool. cp overwrites an identical-named file harmlessly;
# differently-versioned debs land side by side, so old versions are preserved.
for deb in "${debs[@]}"; do
    cp -f "$deb" "$repo_dir/$pool_rel/$(basename "$deb")"
done

# GitHub Pages runs Jekyll by default, which can drop or mangle files; a
# .nojekyll marker at the root disables that so the pool/dists tree is served
# verbatim.
: > "$repo_dir/.nojekyll"

# --- (re)generate the Packages index ---------------------------------------
# Run from the repo root so the emitted Filename fields are repo-root-relative
# (pool/...), which is exactly how apt resolves them against the base URL.
(
    cd "$repo_dir"
    apt-ftparchive packages "$pool_rel" > "$bindir_rel/Packages"
    gzip -9 -n -c "$bindir_rel/Packages" > "$bindir_rel/Packages.gz"
)

# --- (re)generate the Release file -----------------------------------------
# apt-ftparchive release hashes every index under dists/<suite> and emits the
# MD5Sum/SHA256 blocks plus a Date. It must not hash a stale Release, so write
# to a temp file and move it into place afterwards.
release_tmp="$repo_dir/$dist_rel/Release.tmp"
(
    cd "$repo_dir"
    apt-ftparchive \
        -o "APT::FTPArchive::Release::Origin=$origin" \
        -o "APT::FTPArchive::Release::Label=$label" \
        -o "APT::FTPArchive::Release::Suite=$suite" \
        -o "APT::FTPArchive::Release::Codename=$codename" \
        -o "APT::FTPArchive::Release::Architectures=$arch" \
        -o "APT::FTPArchive::Release::Components=$component" \
        -o "APT::FTPArchive::Release::Description=Voisu apt repository" \
        release "$dist_rel" > "$release_tmp"
)
mv -f "$release_tmp" "$repo_dir/$dist_rel/Release"

# --- sign: InRelease (clearsigned) + Release.gpg (detached) -----------------
"${gpg_sign[@]}" --clearsign \
    --output "$repo_dir/$dist_rel/InRelease" \
    "$repo_dir/$dist_rel/Release"
"${gpg_sign[@]}" --detach-sign \
    --output "$repo_dir/$dist_rel/Release.gpg" \
    "$repo_dir/$dist_rel/Release"

# --- publish the public key alongside the packages -------------------------
# Export the ASCII-armored public half of the signing key to the repo root so
# `apt` clients fetch a key that provably matches the signatures just written.
gpg --armor --export "$gpg_key" > "$repo_dir/voisu-archive-keyring.asc"
if ! test -s "$repo_dir/voisu-archive-keyring.asc"; then
    printf 'failed to export the public key for %s\n' "$gpg_key" >&2
    exit 1
fi

# --- self-test: the repo we just wrote must verify -------------------------
# 1) Both signatures verify against the signing key.
if ! gpg --verify "$repo_dir/$dist_rel/InRelease" >/dev/null 2>&1; then
    printf 'self-test FAILED: InRelease clearsignature does not verify\n' >&2
    exit 1
fi
if ! gpg --verify "$repo_dir/$dist_rel/Release.gpg" "$repo_dir/$dist_rel/Release" >/dev/null 2>&1; then
    printf 'self-test FAILED: detached Release.gpg does not verify\n' >&2
    exit 1
fi
# 2) The Release actually pins the Packages we generated (guards against a
#    stale/tampered index): the live sha256 of Packages must appear in Release.
pkg_sha=$(sha256sum "$repo_dir/$bindir_rel/Packages" | awk '{print $1}')
if ! grep -q "$pkg_sha" "$repo_dir/$dist_rel/Release"; then
    printf 'self-test FAILED: Release does not carry the sha256 of the generated Packages index\n' >&2
    exit 1
fi

printf 'published %d package(s) to %s\n' "${#debs[@]}" "$repo_dir"
printf 'signed with key %s: %s\n' "$gpg_key" "$dist_rel/{InRelease,Release,Release.gpg}"
printf 'pool now contains:\n'
ls -1 "$repo_dir/$pool_rel"
