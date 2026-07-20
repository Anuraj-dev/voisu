#!/usr/bin/env bash
set -euo pipefail

# Build a fully self-contained, offline-buildable Voisu source RPM for the
# Fedora COPR channel (GH issue #44, ticket 12).
#
# COPR mock builders have NO network during the RPM build phase, so the whole
# crate graph is vendored (cargo vendor) into a tarball that ships INSIDE the
# .src.rpm as Source1. `rpmbuild -bs` embeds every Source0..SourceN, so the
# resulting SRPM rebuilds with `rpmbuild --rebuild` / mock with networking off:
#   %prep extracts the vendor tarball and writes .cargo/config.toml pointing at
#   it, and %build runs `cargo build --offline`. No crates.io access needed.
#
# This mirrors the dev-machine build-rpm.sh vendoring (byte-reproducible vendor
# archive with an independent-rebuild self-test) and the build-deb.sh guards
# (dirty-tree refusal, VOISU_COMMIT==HEAD, monotonic version scheme with a
# shallow-clone refusal, and an output dir confined to $root/dist/). It touches
# ONE spec (packaging/voisu.spec) shared with build-rpm.sh via the voisu_release
# macro. The pure-validation guards all run before any heavy step.

root=$(realpath "$(git rev-parse --show-toplevel)")
cd "$root"

version=0.1.0

# --- reproducibility guards (mirror build-deb.sh / build-rpm.sh) -----------
requested_commit=${VOISU_COMMIT:-HEAD}
commit=$(git rev-parse --verify "${requested_commit}^{commit}")
if test -n "$(git status --porcelain)"; then
    printf '%s\n' 'refusing to package a dirty checkout; commit the tested tree first' >&2
    exit 1
fi
if test "$(git rev-parse HEAD)" != "$(git rev-parse "$commit")"; then
    printf '%s\n' 'VOISU_COMMIT must be the checked-out commit' >&2
    exit 1
fi

# --- upstream version must match the spec's Version ------------------------
base_version=$(cargo pkgid -p voisu-app | sed -E 's/.*[#@]//')
if test "$base_version" != "$version"; then
    printf 'crate version %s does not match packaged version %s; bump packaging/voisu.spec Version and this script together\n' \
        "$base_version" "$version" >&2
    exit 1
fi

# --- RPM Release scheme (fed to the spec via --define voisu_release) --------
# Every distinct payload gets a STRICTLY INCREASING NEVR so `dnf upgrade` on a
# friend's box always picks the newer build:
#   tagged release  -> voisu_release=N            (VOISU_RPM_RELEASE=N, positive int)
#   snapshot build  -> voisu_release=0.<count>.<ct>.git<sha>
# The leading `0.` guarantees any snapshot sorts BEFORE a matching tagged
# `N%{?dist}` release. The PRIMARY ordering key is the commit count along
# history (strictly increasing for any descendant commit, immune to committer
# clock skew); the committer timestamp is a secondary tiebreaker; the short SHA
# is an identifier only. The count needs full history, so shallow clones are
# refused for snapshots.
if test -n "${VOISU_RPM_RELEASE:-}"; then
    if ! printf '%s' "$VOISU_RPM_RELEASE" | grep -Eq '^[1-9][0-9]*$'; then
        printf 'VOISU_RPM_RELEASE must be a positive integer (got "%s")\n' "$VOISU_RPM_RELEASE" >&2
        exit 1
    fi
    release_tag="v${version}"
    if ! tag_commit=$(git rev-parse --verify --quiet "refs/tags/${release_tag}^{commit}" 2>/dev/null); then
        printf 'release build requires tag %s to exist; create it on the release commit first\n' \
            "$release_tag" >&2
        exit 1
    fi
    if test "$tag_commit" != "$commit"; then
        printf 'release build requires HEAD to be exactly at tag %s (%s), but HEAD is %s\n' \
            "$release_tag" "$tag_commit" "$commit" >&2
        exit 1
    fi
    voisu_release="$VOISU_RPM_RELEASE"
else
    if test "$(git rev-parse --is-shallow-repository)" = "true"; then
        printf '%s\n' 'refusing a snapshot build from a shallow clone: the commit count that orders snapshot releases needs full history (git fetch --unshallow)' >&2
        exit 1
    fi
    ct=$(git show -s --format=%ct "$commit")
    count=$(git rev-list --count "$commit")
    short=$(git rev-parse --short=12 "$commit")
    voisu_release="0.${count}.${ct}.git${short}"
fi

# --- toolchain checks ------------------------------------------------------
for tool in rpmbuild cargo; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        printf '%s not found; install it (rpm-build, cargo) and re-run\n' "$tool" >&2
        exit 1
    fi
done

# --- output dir: canonicalize and confine to $root/dist/ (build-deb.sh) -----
if test -L "$root/dist"; then
    printf 'refusing: %s/dist is a symlink; remove it so the output dir stays inside the tree\n' "$root" >&2
    exit 1
fi
dist_root="$root/dist"
output_dir=${VOISU_SRPM_OUTPUT_DIR:-"$dist_root/srpm"}
output_dir=$(realpath -m "$output_dir")
case "$output_dir" in
    "$dist_root"/?*) : ;;
    *) printf 'refusing to use output dir %s: must be under %s/\n' "$output_dir" "$dist_root" >&2
       exit 1 ;;
esac

# --- scratch build tree; default TMPDIR to /var/tmp (tmpfs quota gotcha) ----
topdir=$(mktemp -d "${TMPDIR:-/var/tmp}/voisu-srpmbuild.XXXXXX")
cleanup() { rm -rf "$topdir"; }
trap cleanup EXIT
mkdir -p "$topdir"/{SOURCES,SPECS,SRPMS}

# --- Source0: the exact-commit git archive ---------------------------------
archive="$topdir/SOURCES/voisu-${version}.tar.gz"
git archive --format=tar.gz --prefix="voisu-${version}/" "$commit" > "$archive"
# List once to a file: `tar -tzf | grep -q` dies of SIGPIPE under pipefail.
tar -tzf "$archive" > "$topdir/source-archive.list"
grep -qx "voisu-${version}/Cargo.lock" "$topdir/source-archive.list"
grep -qx "voisu-${version}/LICENSE" "$topdir/source-archive.list"
grep -qx "voisu-${version}/packaging/voisu.service" "$topdir/source-archive.list"
grep -qx "voisu-${version}/packaging/voisu-overlay.service" "$topdir/source-archive.list"

# --- Source1: byte-reproducible vendor tarball (mirrors build-rpm.sh) ------
# Vendor from an extraction of the exact-commit git archive (never the working
# tree). The deterministic tar/gzip invariants plus an INDEPENDENT re-vendor
# self-test prove cargo vendor output stability, not just tar determinism.
commit_epoch=$(git show -s --format=%ct "$commit")
extract_dir="$topdir/extract"
mkdir -p "$extract_dir"
tar -xzf "$archive" -C "$extract_dir"

vendor_into() {
    # $1 = parent directory to hold voisu-vendor-${version}
    mkdir -p "$1"
    ( cd "$extract_dir/voisu-${version}" \
        && cargo vendor --locked "$1/voisu-vendor-${version}" >/dev/null )
}

deterministic_vendor_archive() {
    # $1 = parent dir containing voisu-vendor-${version}, $2 = output path
    tar --sort=name --mtime="@${commit_epoch}" \
        --owner=0 --group=0 --numeric-owner --mode='u+rw,go=rX' \
        -C "$1" -cf - "voisu-vendor-${version}" | gzip -n > "$2"
}

vendor_into "$topdir/vendor"
vendor_archive="$topdir/SOURCES/voisu-vendor-${version}.tar.gz"
deterministic_vendor_archive "$topdir/vendor" "$vendor_archive"

vendor_into "$topdir/vendor-verify"
repro_archive="$topdir/voisu-vendor-repro.tar.gz"
deterministic_vendor_archive "$topdir/vendor-verify" "$repro_archive"
if ! cmp -s "$vendor_archive" "$repro_archive"; then
    printf '%s\n' 'vendor archive is not reproducible: an independent cargo vendor of the same commit differs' >&2
    exit 1
fi
rm -rf "$topdir/vendor-verify" "$repro_archive"

# Sanity: the ring license texts the spec copies in %prep must exist vendored.
for lic in ring/LICENSE ring/LICENSE-BoringSSL ring/LICENSE-other-bits; do
    if ! test -f "$topdir/vendor/voisu-vendor-${version}/$lic"; then
        printf 'vendored tree is missing %s (spec %%prep copies it into %%license)\n' "$lic" >&2
        exit 1
    fi
done

# Bake the computed Release/commit into the spec as %global. A plain
# `rpmbuild --rebuild` of the resulting SRPM (COPR mock, a friend's box) has no
# access to our --defines, so without this the embedded macros would collapse to
# Release 1.gitunknown; the %global lines make the SRPM self-describing and its
# rebuilds byte-for-byte NEVR-stable. Same mechanism as packaging/copr/make-srpm.sh.
{
    printf '%%global voisu_release %s\n' "$voisu_release"
    printf '%%global voisu_commit %s\n' "$commit"
    cat packaging/voisu.spec
} > "$topdir/SPECS/voisu.spec"

# --- build the source RPM only (embeds Source0 + Source1) ------------------
rpmbuild -bs \
    --define "_topdir $topdir" \
    "$topdir/SPECS/voisu.spec"

rm -rf "$output_dir"
mkdir -p "$output_dir"
find "$topdir/SRPMS" -type f -name '*.src.rpm' -exec cp -t "$output_dir" {} +
printf 'SRPM (Release %s) written to %s\n' "$voisu_release" "$output_dir"
ls -1 "$output_dir"/*.src.rpm
