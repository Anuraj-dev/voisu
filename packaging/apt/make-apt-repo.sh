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
    printf '       VOISU_APT_GPG_KEY=<keyid> %s --refresh <repo_dir>\n' \
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
# Whole-string case globs (NOT grep, which matches line-by-line and would wave an
# embedded-newline value through on one clean line).
is_identifier() {
    case $1 in
        '' ) return 1 ;;
        [!A-Za-z0-9]* ) return 1 ;;          # must start with an alnum
        *[!A-Za-z0-9.+_-]* ) return 1 ;;     # only [A-Za-z0-9.+_-] throughout
        * ) return 0 ;;
    esac
}
# origin/label are Release VALUES only (may contain spaces) but must be a single
# printable line -- any non-printable char (newline/tab/control) is rejected so a
# value cannot forge extra header fields.
is_header_value() {
    case $1 in
        '' ) return 1 ;;
        *[![:print:]]* ) return 1 ;;
        * ) return 0 ;;
    esac
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
# Two modes:
#   publish:  <repo_dir> <deb>...   add/republish one or more new debs.
#   refresh:  --refresh <repo_dir>  re-index + re-sign the EXISTING pool with a
#             fresh Valid-Until and NO new debs. The scheduled apt-refresh
#             workflow (ticket 14) uses this so the signed Release never expires
#             between real releases. It adds nothing, prunes nothing and touches
#             no pool bytes -- only the metadata is regenerated and re-signed,
#             honouring the published-bytes-immutability invariant.
refresh=0
if test "${1:-}" = --refresh; then
    refresh=1
    shift
fi
if test "$refresh" -eq 1; then
    if test "$#" -ne 1; then
        usage
        exit 1
    fi
    repo_dir=$1
    shift
    debs=()
else
    if test "$#" -lt 2; then
        usage
        exit 1
    fi
    repo_dir=$1
    shift
    debs=("$@")
fi

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

# --- .deb validation at the signing boundary (findings 1, 4) ---------------
# Not just "dpkg-deb can parse it": a package we sign into the index must be a
# regular non-symlink file, a real Voisu amd64 package, with a sane Debian
# version and the exact canonical filename its own control fields imply. Used for
# BOTH new inputs AND every pre-existing pool entry, so a contaminated gh-pages
# checkout cannot smuggle a hostile (or superficially-canonical-but-unexpected)
# .deb into the signed Packages.
# Full dpkg version grammar: the upstream part may itself contain hyphens when
# a Debian revision follows (dpkg splits on the LAST hyphen), so a hyphenated
# pre-release base from build-deb.sh (e.g. 0.1.0-rc1-1) must be accepted.
deb_version_ok() { printf '%s' "$1" | grep -Eq '^([0-9]+:)?[0-9][A-Za-z0-9.+~]*(-[A-Za-z0-9.+~]+)*$'; }
validate_deb() {   # $1 = path; echoes canonical basename on success, else fails
    local f=$1 p v a canon
    if test -L "$f"; then printf 'refusing: %s is a symlink\n' "$f" >&2; return 1; fi
    if ! test -f "$f"; then printf 'refusing: %s is not a regular file\n' "$f" >&2; return 1; fi
    case "$f" in *.deb) : ;; *) printf 'refusing: %s does not look like a .deb\n' "$f" >&2; return 1 ;; esac
    p=$(dpkg-deb --field "$f" Package 2>/dev/null) || { printf 'refusing: %s is not a valid Debian package\n' "$f" >&2; return 1; }
    v=$(dpkg-deb --field "$f" Version 2>/dev/null)
    a=$(dpkg-deb --field "$f" Architecture 2>/dev/null)
    if test "$p" != voisu; then printf 'refusing %s: Package is %q, expected voisu\n' "$f" "$p" >&2; return 1; fi
    if test "$a" != "$arch"; then printf 'refusing %s: Architecture is %q, expected %s\n' "$f" "$a" "$arch" >&2; return 1; fi
    if ! deb_version_ok "$v"; then printf 'refusing %s: %q is not a sane Debian version\n' "$f" "$v" >&2; return 1; fi
    canon="${p}_${v}_${a}.deb"
    if test "$(basename "$f")" != "$canon"; then printf 'refusing %s: filename does not match control fields (expected %s)\n' "$f" "$canon" >&2; return 1; fi
    printf '%s' "$canon"
}
# Referenced .deb basenames inside an index object (auto-detects gzip).
index_refs() {
    local f=$1
    if gzip -t "$f" 2>/dev/null; then gzip -cd "$f"; else cat "$f"; fi \
        | awk '/^Filename:/{sub(/^Filename:[ \t]*/,""); sub(/.*\//,""); print}'
}

# Validate the NEW inputs and remember canonical names (the immutability check
# needs them before writing).
declare -a canon_names=()
for deb in "${debs[@]}"; do
    canon=$(validate_deb "$deb") || exit 1
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
# Symlink guards MUST precede the recovery mv below: a contaminated checkout
# shipping dists (or dists/<suite>) as a symlink would otherwise let the
# restore write through it before the commit-phase guards run.
assert_safe_rel 'dists'
assert_safe_rel "$dist_rel"
assert_safe_rel "$pool_rel"

# --- recover, then GC, debris of a hard-crashed prior run --------------------
# A SIGKILL/power loss between the commit's two renames bypasses the EXIT trap
# and leaves the ONLY valid metadata tree at dists/.stage.<random>.old with the
# suite dir absent. Restore it BEFORE garbage collection so a subsequent
# failure in THIS run (contaminated pool, signing error) still leaves the
# previously valid metadata live. Only after the suite dir exists (restored or
# already live) is remaining .stage.*/.keyring.* debris redundant and safe to
# delete (fresh mktemp names each run mean nothing here belongs to this run).
# All under the publisher lock: no other publisher is mid-commit.
if test -d "$repo_dir/dists"; then
    if ! test -d "$repo_dir/$dist_rel"; then
        for d in "$repo_dir"/dists/.stage.*.old; do
            test -d "$d" || continue
            mv "$d" "$repo_dir/$dist_rel"
            printf 'recovered live metadata from crashed publish (%s)\n' "$(basename "$d")" >&2
            break
        done
    fi
    # Plain .stage.* (never-committed staging) and .keyring.* are never valid
    # live metadata -- always safe to collect. The .stage.*.old recovery trees
    # are only redundant once the suite dir exists.
    find "$repo_dir/dists" -maxdepth 1 \
        \( -name '.stage.*' ! -name '.stage.*.old' -o -name '.keyring.*' \) \
        -exec rm -rf {} + 2>/dev/null || true
    if test -d "$repo_dir/$dist_rel"; then
        find "$repo_dir/dists" -maxdepth 1 -name '.stage.*.old' \
            -exec rm -rf {} + 2>/dev/null || true
    fi
fi
mkdir -p "$repo_dir/$pool_rel"

# GitHub Pages runs Jekyll by default; a .nojekyll marker disables that so the
# pool/dists tree is served verbatim.
assert_safe_rel '.nojekyll'
: > "$repo_dir/.nojekyll"

# Add each new deb to the pool under its CANONICAL name. Adding is additive and
# safe: the still-live old index does not reference the new files. Published
# bytes are immutable: an existing same-name file with identical bytes is a
# no-op; with different bytes it is a hard error (finding 3).
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

# --- validate EVERY existing pool entry (finding 1) ------------------------
# After adding the known-good inputs, re-scan the whole pool: a contaminated
# checkout could carry a pre-existing package that would otherwise be indexed
# and signed wholesale. Fail closed on any subdirectory, any non-.deb file, any
# symlink, or anything that is not a canonical Voisu amd64 package. Collect the
# (name, version, path) of every valid entry for the retention decision.
declare -a pool_names=() pool_paths=() pool_versions=()
shopt -s nullglob
for entry in "$repo_dir/$pool_rel"/*; do
    if test -d "$entry"; then
        printf 'refusing: unexpected subdirectory in the pool: %s\n' "$entry" >&2; exit 1
    fi
    case "$entry" in
        *.deb) : ;;
        *) printf 'refusing: unexpected non-.deb entry in the pool: %s\n' "$entry" >&2; exit 1 ;;
    esac
    name=$(validate_deb "$entry") || exit 1
    pool_names+=("$name"); pool_paths+=("$entry")
    pool_versions+=("$(dpkg-deb --field "$entry" Version)")
done
shopt -u nullglob

# --- retention DECISION (finding 5: decide now, delete only after commit) ---
# Choose the newest $keep versions to keep; the rest are pruned, but ONLY after
# the new metadata + signatures + key export + self-tests all succeed. That way
# a failed publish never leaves the still-live index pointing at a deleted .deb.
declare -a keep_names=() prune_paths=()
pool_n=${#pool_paths[@]}
# Refresh mode retains and indexes the ENTIRE existing pool: it publishes nothing
# and must prune nothing, so the metadata refresh never deletes a published .deb.
if test "$refresh" -eq 1; then
    keep=$pool_n
fi
sort_idx=()
for ((i=0; i<pool_n; i++)); do sort_idx+=("$i"); done
for ((x=0; x<pool_n-1; x++)); do
    for ((y=x+1; y<pool_n; y++)); do
        if dpkg --compare-versions "${pool_versions[${sort_idx[$y]}]}" gt "${pool_versions[${sort_idx[$x]}]}"; then
            t=${sort_idx[$x]}; sort_idx[$x]=${sort_idx[$y]}; sort_idx[$y]=$t
        fi
    done
done
for ((r=0; r<pool_n; r++)); do
    gi=${sort_idx[$r]}
    if test "$r" -lt "$keep"; then keep_names+=("${pool_names[$gi]}"); else prune_paths+=("${pool_paths[$gi]}"); fi
done
declare -A keep_set=()
for n in "${keep_names[@]}"; do keep_set["$n"]=1; done

# --- stage metadata in an UNPREDICTABLE, guarded dir (finding 3) -----------
# mktemp yields a random 0700 name (no predictable sibling for a symlink attack),
# and EVERY temp file lives inside it or in a mktemp'd sibling.
assert_safe_rel 'dists'
mkdir -p "$repo_dir/dists"
live_dir="$repo_dir/$dist_rel"
stage_dir=$(mktemp -d "$repo_dir/dists/.stage.XXXXXX")
staged_key=$(mktemp "$repo_dir/dists/.keyring.XXXXXX")
old_dir="${stage_dir}.old"
# Roll back cleanly if anything fails before the commit: only the stage dir, the
# staged key and any freshly-added new debs exist as side effects. The added
# debs are harmless (unreferenced by the still-live old index); nothing else
# live has been mutated -- no old .deb deleted, no metadata swapped, no key
# overwritten. If the commit itself dies between its two renames (live already
# moved aside, stage not yet live), old_dir holds the ONLY copy of the live
# metadata -- restore it instead of deleting it.
cleanup_stage() {
    if test -d "$old_dir" && ! test -d "$live_dir"; then
        mv "$old_dir" "$live_dir" 2>/dev/null || true
    fi
    rm -rf "$stage_dir" "$staged_key" 2>/dev/null || true
    # Delete old_dir only once live metadata verifiably exists (committed or
    # restored); if the restore mv above failed, old_dir is still the only
    # copy -- keep it for the next run's startup recovery.
    if test -d "$live_dir"; then
        rm -rf "$old_dir" 2>/dev/null || true
    fi
}
trap cleanup_stage EXIT
mkdir -p "$stage_dir/$bin_rel"

# Packages: index the whole pool, then WHITELIST only the retained versions, so
# the index never references a to-be-pruned .deb. All temp files stay in stage.
raw_pkgs="$stage_dir/.Packages.raw"
( cd "$repo_dir" && apt-ftparchive packages "$pool_rel" ) > "$raw_pkgs"
keep_list=$(printf '%s\n' "${keep_names[@]}")
awk -v keep="$keep_list" '
    BEGIN{ n=split(keep,a,"\n"); for(i=1;i<=n;i++) if(a[i]!="") K[a[i]]=1; RS=""; FS="\n" }
    { fn=""
      for(i=1;i<=NF;i++) if($i ~ /^Filename:/){ fn=$i; sub(/^Filename:[ \t]*/,"",fn); sub(/.*\//,"",fn) }
      if(fn in K) printf "%s\n\n", $0
    }' "$raw_pkgs" > "$stage_dir/$bin_rel/Packages"
rm -f "$raw_pkgs"
gzip -9 -n -c "$stage_dir/$bin_rel/Packages" > "$stage_dir/$bin_rel/Packages.gz"

# Release over the staged tree (paths relative to the suite dir); temp in stage.
release_tmp="$stage_dir/.Release.raw"
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
# they are not themselves re-listed. Prior digests are carried over for the
# cache-skew window, but ONLY when COHERENT: a prior index object is retained
# solely if every .deb it references is still in the kept set (finding 6). This
# GCs objects that would 404 a pruned .deb and bounds by-hash growth alongside
# the pool.
by_hash_dir="$stage_dir/$bin_rel/by-hash/SHA256"
mkdir -p "$by_hash_dir"
for rel in Packages Packages.gz; do
    dig=$(sha256sum "$stage_dir/$bin_rel/$rel" | awk '{print $1}')
    cp "$stage_dir/$bin_rel/$rel" "$by_hash_dir/$dig"
done
if test -d "$live_dir/$bin_rel/by-hash/SHA256"; then
    for old in "$live_dir/$bin_rel/by-hash/SHA256"/*; do
        test -e "$old" || continue
        b=$(basename "$old")
        test -e "$by_hash_dir/$b" && continue
        coherent=1
        while IFS= read -r ref; do
            test -z "$ref" && continue
            test -n "${keep_set[$ref]:-}" || { coherent=0; break; }
        done < <(index_refs "$old")
        test "$coherent" -eq 1 && cp "$old" "$by_hash_dir/"
    done
fi

# --- export + validate the KEY into the stage BEFORE any live mutation -------
# (finding 5): a failed/empty/multi-primary export must abort before the live
# key or metadata is touched. The served bundle must contain EXACTLY ONE primary
# public key -- our signer -- so a client that (correctly) pins the primary
# fingerprint can trust the whole bundle.
gpg --armor --export "$gpg_key" > "$staged_key"
if ! test -s "$staged_key"; then
    printf 'failed to export the public key for %s\n' "$gpg_key" >&2; exit 1
fi
npub=$(gpg --show-keys --with-colons "$staged_key" | grep -c '^pub:' || true)
if test "$npub" -ne 1; then
    printf 'refusing: exported keyring carries %s primary keys (expected exactly 1)\n' "$npub" >&2; exit 1
fi

# --- COMMIT (finding 5) ----------------------------------------------------
# Everything prospective has succeeded. Now, and only now, mutate live state:
# swap the metadata dir in, install the validated key, and finally delete the
# pruned .debs (which the freshly-live index no longer references). Ordered so a
# mid-sequence failure never leaves the live index pointing at a deleted file.
if test -d "$live_dir"; then
    mv "$live_dir" "$old_dir"
fi
mv "$stage_dir" "$live_dir"
assert_safe_rel 'voisu-archive-keyring.asc'
mv "$staged_key" "$repo_dir/voisu-archive-keyring.asc"
rm -rf "$old_dir"
for p in "${prune_paths[@]}"; do
    printf 'retention: removing old pool package %s\n' "$(basename "$p")"
    rm -f "$p"
done
trap - EXIT

if test "$refresh" -eq 1; then
    printf 'refreshed metadata for %s (re-signed, valid %d days; %d pool package(s) retained)\n' \
        "$repo_dir" "$valid_days" "$pool_n"
else
    printf 'published %d package(s) to %s (keep=%d, valid %d days)\n' \
        "${#debs[@]}" "$repo_dir" "$keep" "$valid_days"
fi
printf 'signed with key %s: %s\n' "$gpg_key" "$dist_rel/{InRelease,Release,Release.gpg}"
printf 'pool now contains:\n'
ls -1 "$repo_dir/$pool_rel"
