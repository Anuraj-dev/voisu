#!/usr/bin/env bash
# Shared helpers for the Voisu RPM build scripts (ticket 12, GH issue #44).
#
# Sourced by:
#   - packaging/build-rpm.sh   (dev-machine RPM, rpmbuild -ba)
#   - packaging/build-srpm.sh  (local offline-buildable SRPM, rpmbuild -bs)
#   - packaging/copr/make-srpm.sh (COPR Custom source method)
#
# Centralises the version derivation, the unified Release policy, the
# byte-reproducible vendor tarball + independent re-vendor self-test, and the
# path-confinement guard, so the COPR path can never silently skip a check the
# local paths perform. Every function is pure w.r.t. globals and takes its inputs
# as arguments.

# --- unified Release policy -------------------------------------------------
# ALL pre-release builds (dev machine AND COPR snapshots) use
#   0.<count>.<ct>.git<sha>
# and tagged releases use a plain committed integer N (packaging/rpm-release),
# so NEVRs order correctly across every path:
#   0.<count>...           (pre-release, leading 0. keeps it below any release)
#   N                      (tagged release, N >= 1)
# The primary ordering key for pre-release builds is the commit COUNT along
# history (strictly increasing for any descendant commit, immune to committer
# clock skew); the committer timestamp is a secondary tiebreaker and the short
# SHA an identifier only. rpm compares 0.<...> below 1 numerically on the first
# segment, so every snapshot sorts below the first tagged release.

# Derive the crate version from cargo metadata for a checkout and verify it
# matches that checkout's spec Version. Echoes the version.
#   $1 = checkout root (contains the workspace + packaging/voisu.spec)
voisu_derive_version() {
    local root=$1 v spec_v
    v=$(cd "$root" && cargo pkgid -p voisu-app | sed -E 's/.*[#@]//')
    if ! printf '%s' "$v" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
        printf 'could not derive a clean semver version from cargo metadata (got "%s")\n' "$v" >&2
        return 1
    fi
    spec_v=$(grep -E '^Version:[[:space:]]' "$root/packaging/voisu.spec" | awk '{print $2}')
    if test "$v" != "$spec_v"; then
        printf 'crate version %s != spec Version %s; bump crates/voisu-app/Cargo.toml and packaging/voisu.spec together\n' \
            "$v" "$spec_v" >&2
        return 1
    fi
    printf '%s' "$v"
}

# The committed tagged-release number. Bump packaging/rpm-release to respin a
# tagged release (0.1.0-2) without moving the tag.
#   $1 = checkout root
voisu_tag_release_number() {
    local root=$1 n
    n=$(tr -d '[:space:]' < "$root/packaging/rpm-release")
    if ! printf '%s' "$n" | grep -Eq '^[1-9][0-9]*$'; then
        printf 'packaging/rpm-release must contain a positive integer (got "%s")\n' "$n" >&2
        return 1
    fi
    printf '%s' "$n"
}

# Compute the Release string (no %%{?dist}; rpm appends that).
#   $1 = checkout root   $2 = commit   $3 = tagged? (yes/no)   $4 = tag number
voisu_compute_release() {
    local root=$1 commit=$2 tagged=$3 tagn=$4
    if test "$tagged" = yes; then
        printf '%s' "$tagn"
        return 0
    fi
    local ct count short
    ct=$(git -C "$root" show -s --format=%ct "$commit")
    count=$(git -C "$root" rev-list --count "$commit")
    short=$(git -C "$root" rev-parse --short=12 "$commit")
    printf '0.%s.%s.git%s' "$count" "$ct" "$short"
}

# --- path confinement (mirrors build-deb.sh / build-srpm.sh) ----------------
# Resolve $2, require it strictly beneath the real $1 (rejecting symlink escapes
# and $1 itself), and echo the canonical path. $1 must already be canonical and
# a non-symlink directory.
#   $1 = confinement base   $2 = candidate path
voisu_confine_under() {
    local base=$1 cand=$2 resolved
    if test -L "$base"; then
        printf 'refusing: %s is a symlink; remove it so the output stays inside the tree\n' "$base" >&2
        return 1
    fi
    resolved=$(realpath -m "$cand")
    case "$resolved" in
        "$base"/?*) printf '%s' "$resolved" ;;
        *) printf 'refusing to use %s: must be strictly under %s/\n' "$resolved" "$base" >&2
           return 1 ;;
    esac
}

# --- vendoring --------------------------------------------------------------
# Deterministic vendor archive: --sort fixes order, --owner/--group/--numeric-owner
# fix ownership, --mtime fixes timestamps, --mode normalises perms, gzip -n drops
# the name/timestamp. Byte-identical for identical input.
#   $1 = parent dir containing voisu-vendor-<ver>   $2 = version
#   $3 = mtime epoch   $4 = output tarball path
voisu_deterministic_vendor_archive() {
    local parent=$1 version=$2 epoch=$3 out=$4
    tar --sort=name --mtime="@${epoch}" \
        --owner=0 --group=0 --numeric-owner --mode='u+rw,go=rX' \
        -C "$parent" -cf - "voisu-vendor-${version}" | gzip -n > "$out"
}

# Vendor an extracted source tree, verify reproducibility with an INDEPENDENT
# re-vendor byte-compare, and run the source + ring-license sanity checks that
# %prep depends on. Writes the vendor tarball to $5.
#   $1 = extracted source dir (the voisu-<ver> directory)
#   $2 = version   $3 = commit epoch   $4 = scratch parent dir
#   $5 = output vendor tarball path
voisu_vendor_and_verify() {
    local src=$1 version=$2 epoch=$3 scratch=$4 out=$5

    # Source sanity: the files %prep / %files reference must be present.
    local f
    for f in Cargo.lock LICENSE packaging/voisu.service packaging/voisu-overlay.service; do
        if ! test -e "$src/$f"; then
            printf 'source tree is missing %s\n' "$f" >&2
            return 1
        fi
    done

    _voisu_vendor_into() {
        # $1 = parent dir to hold voisu-vendor-<ver>
        mkdir -p "$1"
        ( cd "$src" && cargo vendor --locked "$1/voisu-vendor-${version}" >/dev/null )
    }

    mkdir -p "$scratch"
    _voisu_vendor_into "$scratch/vendor"
    voisu_deterministic_vendor_archive "$scratch/vendor" "$version" "$epoch" "$out"

    # Independent re-vendor of the same tree must yield a byte-identical archive.
    _voisu_vendor_into "$scratch/vendor-verify"
    local repro="$scratch/vendor-repro.tar.gz"
    voisu_deterministic_vendor_archive "$scratch/vendor-verify" "$version" "$epoch" "$repro"
    if ! cmp -s "$out" "$repro"; then
        printf 'vendor archive is not reproducible: an independent cargo vendor of the same commit differs\n' >&2
        return 1
    fi
    rm -rf "$scratch/vendor-verify" "$repro"

    # Ring license texts the spec copies in %prep must exist vendored.
    local lic
    for lic in ring/LICENSE ring/LICENSE-BoringSSL ring/LICENSE-other-bits; do
        if ! test -f "$scratch/vendor/voisu-vendor-${version}/$lic"; then
            printf 'vendored tree is missing %s (spec %%prep copies it into %%license)\n' "$lic" >&2
            return 1
        fi
    done
    unset -f _voisu_vendor_into
}

# --- release-ordering self-test ---------------------------------------------
# Echo -1/0/1 for labelCompare(0:$version-$r1, 0:$version-$r2). Prefers the
# python3 rpm module, falls back to rpmdev-vercmp, echoes empty if neither.
#   $1 = version   $2 = release r1   $3 = release r2
voisu_vercmp() {
    local version=$1 r1=$2 r2=$3
    if command -v python3 >/dev/null 2>&1 && python3 -c 'import rpm' >/dev/null 2>&1; then
        python3 - "$version" "$r1" "$r2" <<'PY'
import sys, rpm
v = sys.argv[1]
print(rpm.labelCompare(("0", v, sys.argv[2]), ("0", v, sys.argv[3])))
PY
    elif command -v rpmdev-vercmp >/dev/null 2>&1; then
        if rpmdev-vercmp "0:${version}-${r1}" "0:${version}-${r2}" >/dev/null 2>&1; then
            echo 0
        else
            case $? in 11) echo 1 ;; 12) echo -1 ;; *) echo "" ;; esac
        fi
    else
        echo ""
    fi
}

# Assert the unified Release policy orders correctly. No-op with a warning if no
# vercmp tool is available.
#   $1 = version
voisu_assert_release_ordering() {
    local version=$1 a b want got
    if test -z "$(voisu_vercmp "$version" 1 1)"; then
        printf 'note: no rpm vercmp tool (python3-rpm or rpmdev-vercmp); skipping release-ordering self-test\n' >&2
        return 0
    fi
    # each row: r1 r2 expected(-1 means r1<r2)
    local rows=(
        '0.100.1000.gitaaaaaaaaaaaa 0.200.2000.gitbbbbbbbbbbbb -1'  # old dev < new dev (count)
        '0.200.2000.gitbbbbbbbbbbbb 1 -1'                            # snapshot < tagged
        '1 2 -1'                                                     # tagged respin
        '0.200.2000.gitbbbbbbbbbbbb.fc43 0.200.2000.gitbbbbbbbbbbbb.fc44 -1'  # dist transition
        '0.200.2000.gitbbbbbbbbbbbb.fc43 1.fc43 -1'                  # snapshot < tagged (same dist)
    )
    local row
    for row in "${rows[@]}"; do
        # shellcheck disable=SC2086
        set -- $row
        a=$1; b=$2; want=$3
        got=$(voisu_vercmp "$version" "$a" "$b")
        if test "$got" != "$want"; then
            printf 'release-ordering self-test FAILED: cmp(%s, %s) = %s, want %s\n' "$a" "$b" "$got" "$want" >&2
            return 1
        fi
    done
    printf 'release-ordering self-test: %d cases pass\n' "${#rows[@]}" >&2
}
