#!/usr/bin/env bash
set -euo pipefail

# Build a fully self-contained, offline-buildable Voisu source RPM for the
# Fedora COPR channel (GH issue #44, ticket 12).
#
# COPR mock builders have NO network during the RPM build phase, so the whole
# crate graph is vendored (cargo vendor) into a tarball that ships INSIDE the
# .src.rpm as Source1. `rpmbuild -bs` embeds every Source0..SourceN, so the
# resulting SRPM rebuilds with `rpmbuild --rebuild` / mock with networking off:
# %prep extracts the vendor tarball and writes .cargo/config.toml pointing at it,
# and %build runs `cargo build --offline`. No crates.io access needed.
#
# The computed Release and commit are BAKED into the SRPM's embedded spec as
# %global (not passed via --define), because a downstream `rpmbuild --rebuild`
# (COPR mock, a friend's box) has no access to our defines; the %global lines
# make the SRPM self-describing and its rebuilds NEVR-stable. Shared logic
# (version derivation, unified Release policy, vendor+verify, path confinement)
# lives in packaging/rpm-lib.sh so the COPR make-srpm.sh runs identical checks.

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=packaging/rpm-lib.sh
source "$script_dir/rpm-lib.sh"

root=$(realpath "$(git rev-parse --show-toplevel)")
cd "$root"

# --- toolchain checks (before any use) -------------------------------------
for tool in git cargo rpmbuild tar gzip; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        printf '%s not found; install it (git, cargo, rpm-build, tar, gzip) and re-run\n' "$tool" >&2
        exit 1
    fi
done

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
if test "$(git rev-parse --is-shallow-repository)" = "true"; then
    printf '%s\n' 'refusing a build from a shallow clone: the commit count that orders pre-release versions needs full history (git fetch --unshallow)' >&2
    exit 1
fi

# --- version from cargo metadata, verified against the spec ----------------
version=$(voisu_derive_version "$root")

# --- unified Release policy (packaging/rpm-lib.sh) -------------------------
# Tagged release iff HEAD sits exactly on tag v<version>; else a snapshot.
if tag_commit=$(git rev-parse --verify --quiet "refs/tags/v${version}^{commit}" 2>/dev/null) \
        && test "$tag_commit" = "$commit"; then
    tag_number=$(voisu_tag_release_number "$root")
    voisu_release=$(voisu_compute_release "$root" "$commit" yes "$tag_number")
else
    voisu_release=$(voisu_compute_release "$root" "$commit" no "")
fi
voisu_assert_release_ordering "$version"

# --- output dir: confine strictly under $root/dist/ ------------------------
dist_root="$root/dist"
output_dir=$(voisu_confine_under "$dist_root" "${VOISU_SRPM_OUTPUT_DIR:-$dist_root/srpm}")

# --- scratch build tree; default TMPDIR to /var/tmp (tmpfs quota gotcha) ----
topdir=$(mktemp -d "${TMPDIR:-/var/tmp}/voisu-srpmbuild.XXXXXX")
cleanup() { rm -rf "$topdir"; }
trap cleanup EXIT
mkdir -p "$topdir"/{SOURCES,SPECS,SRPMS}

# --- Source0: the exact-commit git archive; work only from its extraction --
archive="$topdir/SOURCES/voisu-${version}.tar.gz"
git archive --format=tar.gz --prefix="voisu-${version}/" "$commit" > "$archive"
extract_dir="$topdir/extract"
mkdir -p "$extract_dir"
tar -xzf "$archive" -C "$extract_dir"
src="$extract_dir/voisu-${version}"

# --- Source1: byte-reproducible vendor tarball + self-test + sanity --------
commit_epoch=$(git show -s --format=%ct "$commit")
vendor_archive="$topdir/SOURCES/voisu-vendor-${version}.tar.gz"
voisu_vendor_and_verify "$src" "$version" "$commit_epoch" "$topdir/scratch" "$vendor_archive"

# --- spec: read from the extracted exact-commit tree, bake Release/commit ---
{
    printf '%%global voisu_release %s\n' "$voisu_release"
    printf '%%global voisu_commit %s\n' "$commit"
    cat "$src/packaging/voisu.spec"
} > "$topdir/SPECS/voisu.spec"

# --- build the source RPM only (embeds Source0 + Source1) ------------------
rpmbuild -bs --define "_topdir $topdir" "$topdir/SPECS/voisu.spec"

rm -rf "$output_dir"
mkdir -p "$output_dir"
find "$topdir/SRPMS" -type f -name '*.src.rpm' -exec cp -t "$output_dir" {} +
printf 'SRPM (Release %s) written to %s\n' "$voisu_release" "$output_dir"
ls -1 "$output_dir"/*.src.rpm
