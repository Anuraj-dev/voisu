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
# Release.gpg. It is idempotent: re-running against a populated repo keeps
# published versions (retained per policy) and regenerates + re-signs the
# indices, so a newer version is added alongside the old ones and `apt upgrade`
# sees it.
#
# Index generation uses apt-ftparchive (from apt-utils), NOT dpkg-scanpackages:
# apt-ftparchive emits both the Packages index and, in one pass, the Release
# file with the MD5Sum/SHA256 blocks over every index -- exactly what a signed
# repo needs.
#
# HARDENING (ticket 13 review round 1). Every identity field that lands in a
# filesystem path or the Release header is validated (no traversal, whitespace,
# or control chars). Destination paths are checked component-by-component for
# symlinks so a hostile gh-pages checkout (e.g. `dists -> /victim`) cannot
# redirect a write or an rm. Published .deb bytes are IMMUTABLE: re-publishing a
# version with different content is a hard error. The .deb signing boundary
# validates the control fields (Package=voisu, Architecture=amd64, sane version)
# and the canonical filename, not just that dpkg-deb can parse the archive. The
# whole metadata set is staged in a sibling dir, self-tested (exact SHA256 block
# match + signature verification), and swapped in under an exclusive lock so a
# directly-served checkout never exposes half-written or mis-signed metadata.
# Indices are also published content-addressed (by-hash) with Acquire-By-Hash,
# and Release carries a Valid-Until to bound replay/freeze.
#
# SECRETS. The real signing key lives only as CI secrets (GPG_PRIVATE_KEY /
# GPG_PASSPHRASE) and in Raja's offline storage; this script never sees, prints,
# or writes a private key or passphrase. It signs with whatever secret key id is
# named by $VOISU_APT_GPG_KEY, using the gpg keyring already present in the
# environment ($GNUPGHOME). CI imports the private key into an ephemeral
# GNUPGHOME and points VOISU_APT_GPG_PASSPHRASE_FILE at a passphrase file for
# loopback pinentry; local verification uses a throwaway key with no passphrase.
# The PUBLIC key of the signing key is (re-)exported to the repo root as
# voisu-archive-keyring.asc so friends fetch a key that provably matches the
# signatures just written (see README: friends still fingerprint-pin it).

# --- usage -----------------------------------------------------------------
# VOISU_APT_GPG_KEY=<keyid> packaging/apt/make-apt-repo.sh <repo_dir> <deb>...
#
#   <repo_dir>  absolute path to the repo working directory (the gh-pages tree).
#   <deb>...    one or more .deb files to (re)publish into the pool.
#
# Environment:
#   VOISU_APT_GPG_KEY              signing key id/fingerprint
#                                  (default: the Voisu package-signing fingerprint)
#   VOISU_APT_GPG_PASSPHRASE_FILE  optional file with the key passphrase; enables
#                                  gpg loopback pinentry (CI).
#   VOISU_APT_KEEP                 pool retention: total versions to keep
#                                  (default 3 = current + 2 prior).
#   VOISU_APT_VALID_DAYS           Release Valid-Until horizon in days (default 30).
#   VOISU_APT_ORIGIN / _LABEL / _SUITE / _CODENAME / _COMPONENT
#                                  override the Release identity fields.

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
keep=${VOISU_APT_KEEP:-3}
valid_days=${VOISU_APT_VALID_DAYS:-30}

# --- field validation (finding 1) ------------------------------------------
# suite/codename/component land inside filesystem paths and the Release header,
# so they must be conservative Debian identifiers: a leading alnum then only
# [A-Za-z0-9.+_-]. This rejects '', '/', '..', whitespace and control chars.
is_identifier() { printf '%s' "$1" | grep -Eq '^[A-Za-z0-9][A-Za-z0-9.+_-]*$'; }
# origin/label are Release VALUES only (may contain spaces) but must be a single
# printable line -- no newlines/control chars that could forge header fields.
is_header_value() {
    printf '%s' "$1" | grep -Eq '^[[:print:]]+$' && ! printf '%s' "$1" | grep -q '[[:cntrl:]]'
}

for pair in "suite=$suite" "codename=$codename" "component=$component"; do
    name=${pair%%=*}; val=${pair#*=}
    if ! is_identifier "$val"; then
        printf 'refusing: %s value %q is not a safe Debian identifier ([A-Za-z0-9][A-Za-z0-9.+_-]*)\n' \
            "$name" "$val" >&2
        exit 1
    fi
done
for pair in "origin=$origin" "label=$label"; do
    name=${pair%%=*}; val=${pair#*=}
    if ! is_header_value "$val"; then
        printf 'refusing: %s value must be a single printable line\n' "$name" >&2
        exit 1
    fi
done
if ! printf '%s' "$keep" | grep -Eq '^[1-9][0-9]*$'; then
    printf 'refusing: VOISU_APT_KEEP must be a positive integer (got %q)\n' "$keep" >&2
    exit 1
fi
if ! printf '%s' "$valid_days" | grep -Eq '^[1-9][0-9]*$'; then
    printf 'refusing: VOISU_APT_VALID_DAYS must be a positive integer (got %q)\n' "$valid_days" >&2
    exit 1
fi

# --- argument parsing ------------------------------------------------------
if test "$#" -lt 2; then
    usage
    exit 1
fi
repo_dir=$1
shift
debs=("$@")

# --- tool checks (fail closed) ---------------------------------------------
for tool in apt-ftparchive gpg gzip dpkg-deb dpkg sha256sum stat flock date awk; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        printf 'required tool %s not found; on Ubuntu/Debian: apt-get install apt-utils gnupg dpkg-dev coreutils util-linux\n' \
            "$tool" >&2
        exit 1
    fi
done

# --- repo_dir confinement (symlink-proof, no system dirs) ------------------
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
mkdir -p "$repo_dir"

# --- per-destination symlink guard (finding 2) -----------------------------
# Reject a symlink at ANY component of a repo-relative destination path, so a
# pre-existing symlink below repo_dir (dists, pool, an index file, a .deb dest,
# the keyring) cannot redirect our writes or our rm. Callers pass a path that is
# relative to repo_dir.
assert_safe_rel() {
    local rel=$1 cur=$repo_dir comp
    case "$rel" in
        /* | *..* ) printf 'refusing unsafe relative path: %s\n' "$rel" >&2; exit 1 ;;
    esac
    local IFS=/
    for comp in $rel; do
        test -z "$comp" && continue
        cur="$cur/$comp"
        if test -L "$cur"; then
            printf 'refusing: %s is a symlink; a repo destination must not traverse symlinks\n' "$cur" >&2
            exit 1
        fi
    done
}

# --- validate the .deb inputs at the signing boundary (finding 4) ----------
# Not just "dpkg-deb can parse it": extract and check the control fields and the
# canonical filename. Everything we sign must be a real Voisu amd64 package.
deb_version_ok() { printf '%s' "$1" | grep -Eq '^([0-9]+:)?[0-9][A-Za-z0-9.+~]*(-[A-Za-z0-9.+~]+)?$'; }
declare -a canon_names=()
for deb in "${debs[@]}"; do
    if ! test -f "$deb"; then
        printf 'refusing: %s is not a regular file\n' "$deb" >&2
        exit 1
    fi
    case "$deb" in *.deb) : ;; *) printf 'refusing: %s does not look like a .deb\n' "$deb" >&2; exit 1 ;; esac
    p=$(dpkg-deb --field "$deb" Package 2>/dev/null) || { printf 'refusing: %s is not a valid Debian package\n' "$deb" >&2; exit 1; }
    v=$(dpkg-deb --field "$deb" Version 2>/dev/null)
    a=$(dpkg-deb --field "$deb" Architecture 2>/dev/null)
    if test "$p" != voisu; then
        printf 'refusing %s: Package is %q, expected voisu\n' "$deb" "$p" >&2; exit 1
    fi
    if test "$a" != "$arch"; then
        printf 'refusing %s: Architecture is %q, expected %s\n' "$deb" "$a" "$arch" >&2; exit 1
    fi
    if ! deb_version_ok "$v"; then
        printf 'refusing %s: %q is not a sane Debian version\n' "$deb" "$v" >&2; exit 1
    fi
    canon="${p}_${v}_${a}.deb"
    if test "$(basename "$deb")" != "$canon"; then
        printf 'refusing %s: filename does not match control fields (expected %s)\n' "$deb" "$canon" >&2; exit 1
    fi
    canon_names+=("$canon")
done
# Reject two inputs that claim the same (package,version,arch) but differ.
for i in "${!debs[@]}"; do
    for j in "${!debs[@]}"; do
        test "$j" -le "$i" && continue
        if test "${canon_names[$i]}" = "${canon_names[$j]}" && ! cmp -s "${debs[$i]}" "${debs[$j]}"; then
            printf 'refusing: two inputs share %s but differ in content\n' "${canon_names[$i]}" >&2
            exit 1
        fi
    done
done

# --- verify the signing key is usable (fail closed before touching files) --
if ! gpg --list-secret-keys "$gpg_key" >/dev/null 2>&1; then
    printf 'signing key %s not found in the gpg keyring ($GNUPGHOME); import it first\n' "$gpg_key" >&2
    exit 1
fi
gpg_sign=(gpg --batch --yes --local-user "$gpg_key" --armor)
if test -n "${VOISU_APT_GPG_PASSPHRASE_FILE:-}"; then
    if ! test -f "$VOISU_APT_GPG_PASSPHRASE_FILE"; then
        printf 'VOISU_APT_GPG_PASSPHRASE_FILE points at %s which does not exist\n' \
            "$VOISU_APT_GPG_PASSPHRASE_FILE" >&2
        exit 1
    fi
    gpg_sign+=(--pinentry-mode loopback --passphrase-file "$VOISU_APT_GPG_PASSPHRASE_FILE")
fi

# --- exclusive publisher lock (finding 9) ----------------------------------
assert_safe_rel '.publish.lock'
exec 9>"$repo_dir/.publish.lock"
if ! flock -w 60 9; then
    printf 'refusing: another publisher holds the lock on %s\n' "$repo_dir" >&2
    exit 1
fi

# --- lay out the pool ------------------------------------------------------
pool_rel="pool/${component}/v/voisu"
dist_rel="dists/${suite}"
bin_rel="${component}/binary-${arch}"
assert_safe_rel "$pool_rel"
assert_safe_rel "$dist_rel"
mkdir -p "$repo_dir/$pool_rel"

# GitHub Pages runs Jekyll by default; a .nojekyll marker disables that so the
# pool/dists tree is served verbatim.
assert_safe_rel '.nojekyll'
: > "$repo_dir/.nojekyll"

# Add each deb to the pool under its CANONICAL name. Published bytes are
# immutable: an existing same-name file with identical bytes is a no-op; with
# different bytes it is a hard error (finding 3).
for idx in "${!debs[@]}"; do
    deb=${debs[$idx]}
    canon=${canon_names[$idx]}
    dest_rel="$pool_rel/$canon"
    assert_safe_rel "$dest_rel"
    dest="$repo_dir/$dest_rel"
    if test -e "$dest"; then
        if cmp -s "$deb" "$dest"; then
            printf 'pool already holds identical %s (idempotent)\n' "$canon"
            continue
        fi
        printf 'refusing: %s is already published with DIFFERENT bytes; published versions are immutable -- bump the version\n' \
            "$canon" >&2
        exit 1
    fi
    cp "$deb" "$dest"
done

# --- retention (finding 16) ------------------------------------------------
# Keep the newest $keep versions of voisu in the pool by Debian version order;
# delete only the older, now-unreferenced .debs BEFORE regenerating metadata, so
# no Packages entry ever points at a removed file. gh-pages/Pages storage stays
# bounded.
prune_pool() {
    local dir="$repo_dir/$pool_rel"
    local -a files=()
    local f
    for f in "$dir"/*.deb; do test -e "$f" && files+=("$f"); done
    test "${#files[@]}" -le "$keep" && return 0
    # selection sort by Debian version (descending) using dpkg --compare-versions.
    local i j tmp vi vj n=${#files[@]}
    for ((i=0; i<n-1; i++)); do
        for ((j=i+1; j<n; j++)); do
            vi=$(dpkg-deb --field "${files[$i]}" Version)
            vj=$(dpkg-deb --field "${files[$j]}" Version)
            if dpkg --compare-versions "$vj" gt "$vi"; then
                tmp=${files[$i]}; files[$i]=${files[$j]}; files[$j]=$tmp
            fi
        done
    done
    for ((i=keep; i<n; i++)); do
        printf 'retention: removing old pool package %s\n' "$(basename "${files[$i]}")"
        rm -f "${files[$i]}"
    done
}
prune_pool

# --- stage the metadata, self-test, swap atomically (findings 9,10,11,12) --
live_dir="$repo_dir/$dist_rel"
stage_rel="dists/.stage.${suite}.$$"
old_rel="dists/.old.${suite}.$$"
assert_safe_rel "$stage_rel"
assert_safe_rel "$old_rel"
stage_dir="$repo_dir/$stage_rel"
rm -rf "$stage_dir"
mkdir -p "$stage_dir/$bin_rel"

# Packages index (Filename fields are repo-root-relative -> pool/...).
( cd "$repo_dir" && apt-ftparchive packages "$pool_rel" ) > "$stage_dir/$bin_rel/Packages"
gzip -9 -n -c "$stage_dir/$bin_rel/Packages" > "$stage_dir/$bin_rel/Packages.gz"

# Release over the staged tree (paths relative to the suite dir).
release_tmp="$repo_dir/dists/.release.$$"
( apt-ftparchive \
    -o "APT::FTPArchive::Release::Origin=$origin" \
    -o "APT::FTPArchive::Release::Label=$label" \
    -o "APT::FTPArchive::Release::Suite=$suite" \
    -o "APT::FTPArchive::Release::Codename=$codename" \
    -o "APT::FTPArchive::Release::Architectures=$arch" \
    -o "APT::FTPArchive::Release::Components=$component" \
    -o "APT::FTPArchive::Release::Description=Voisu apt repository" \
    release "$stage_dir" ) > "$release_tmp"

# Bound replay/freeze with Valid-Until, and advertise by-hash. Insert both after
# the Date: line (header, before the checksum blocks).
valid_until=$(date -u -d "+${valid_days} days" '+%a, %d %b %Y %H:%M:%S %z')
awk -v vu="Valid-Until: $valid_until" -v bh='Acquire-By-Hash: yes' '
    {print}
    /^Date:/{print bh; print vu}
' "$release_tmp" > "$stage_dir/Release"
rm -f "$release_tmp"

# Sign the staged Release.
"${gpg_sign[@]}" --clearsign --output "$stage_dir/InRelease" "$stage_dir/Release"
"${gpg_sign[@]}" --detach-sign --output "$stage_dir/Release.gpg" "$stage_dir/Release"

# --- self-test the STAGED metadata (finding 12) ----------------------------
# Signatures verify.
if ! gpg --verify "$stage_dir/InRelease" >/dev/null 2>&1; then
    printf 'self-test FAILED: InRelease clearsignature does not verify\n' >&2; exit 1
fi
if ! gpg --verify "$stage_dir/Release.gpg" "$stage_dir/Release" >/dev/null 2>&1; then
    printf 'self-test FAILED: detached Release.gpg does not verify\n' >&2; exit 1
fi
# The SHA256 block pins EXACTLY (digest, size, path) for both index forms.
assert_release_hashes() {
    local release=$1 base=$2 rel dig size
    for rel in "$bin_rel/Packages" "$bin_rel/Packages.gz"; do
        dig=$(sha256sum "$base/$rel" | awk '{print $1}')
        size=$(stat -c %s "$base/$rel")
        if ! awk -v d="$dig" -v s="$size" -v p="$rel" '
            /^SHA256:/{insec=1; next}
            /^[^ ]/{insec=0}
            insec && $1==d && $2==s && $3==p {ok=1}
            END{exit !ok}' "$release"; then
            printf 'self-test FAILED: Release SHA256 block lacks exact entry for %s (%s %s)\n' \
                "$rel" "$dig" "$size" >&2
            exit 1
        fi
    done
}
assert_release_hashes "$stage_dir/Release" "$stage_dir"

# by-hash: content-addressed copies of each index, published AFTER Release so
# they are not themselves re-listed. Prior digests are carried over on swap so
# clients mid-fetch during cache skew still resolve (Acquire-By-Hash).
by_hash_dir="$stage_dir/$bin_rel/by-hash/SHA256"
mkdir -p "$by_hash_dir"
for rel in Packages Packages.gz; do
    dig=$(sha256sum "$stage_dir/$bin_rel/$rel" | awk '{print $1}')
    cp "$stage_dir/$bin_rel/$rel" "$by_hash_dir/$dig"
done
# Carry over the previous by-hash objects (retain old index digests) without
# clobbering the new ones.
if test -d "$live_dir/$bin_rel/by-hash/SHA256"; then
    for old in "$live_dir/$bin_rel/by-hash/SHA256"/*; do
        test -e "$old" || continue
        test -e "$by_hash_dir/$(basename "$old")" || cp "$old" "$by_hash_dir/"
    done
fi

# Atomic-ish swap under the held lock: move any live suite dir aside, move the
# staged one in, drop the old. The gap is microseconds and serialized by flock;
# ticket 14's publish workflow should additionally use a GitHub Actions
# concurrency group so two release jobs never race the gh-pages branch.
if test -d "$live_dir"; then
    mv "$live_dir" "$repo_dir/$old_rel"
fi
mv "$stage_dir" "$live_dir"
rm -rf "$repo_dir/$old_rel"

# --- publish the public key alongside the packages -------------------------
assert_safe_rel 'voisu-archive-keyring.asc'
gpg --armor --export "$gpg_key" > "$repo_dir/voisu-archive-keyring.asc"
if ! test -s "$repo_dir/voisu-archive-keyring.asc"; then
    printf 'failed to export the public key for %s\n' "$gpg_key" >&2; exit 1
fi

printf 'published %d package(s) to %s (keep=%d, valid %d days)\n' \
    "${#debs[@]}" "$repo_dir" "$keep" "$valid_days"
printf 'signed with key %s: %s\n' "$gpg_key" "$dist_rel/{InRelease,Release,Release.gpg}"
printf 'pool now contains:\n'
ls -1 "$repo_dir/$pool_rel"
