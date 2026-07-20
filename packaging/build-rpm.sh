#!/usr/bin/env bash
set -euo pipefail

# Dev-machine Fedora RPM: tests the exact commit, builds it, and packages binary
# RPMs via rpmbuild -ba from a Cargo.lock-pinned, byte-reproducible vendored
# source archive. Shares the version derivation, unified Release policy, and
# vendor+verify logic with the COPR path via packaging/rpm-lib.sh.

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=packaging/rpm-lib.sh
source "$script_dir/rpm-lib.sh"

root=$(git rev-parse --show-toplevel)
cd "$root"

requested_commit=${VOISU_COMMIT:-HEAD}
commit=$(git rev-parse --verify "${requested_commit}^{commit}")
output_dir=${VOISU_RPM_OUTPUT_DIR:-"$root/dist/rpm"}

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

# Version from cargo metadata (verified against the spec); unified Release policy.
version=$(voisu_derive_version "$root")
if tag_commit=$(git rev-parse --verify --quiet "refs/tags/v${version}^{commit}" 2>/dev/null) \
        && test "$tag_commit" = "$commit"; then
    tag_number=$(voisu_tag_release_number "$root")
    voisu_release=$(voisu_compute_release "$root" "$commit" yes "$tag_number")
else
    voisu_release=$(voisu_compute_release "$root" "$commit" no "")
fi
voisu_assert_release_ordering "$version"

printf 'Testing exact Voisu commit: %s (Release %s)\n' "$commit" "$voisu_release"
cargo test --locked --workspace
cargo build --locked --release --workspace
cargo check --locked -p voisu-app --features overlay

topdir=$(mktemp -d "${TMPDIR:-/tmp}/voisu-rpmbuild.XXXXXX")
cleanup() { rm -rf "$topdir"; }
trap cleanup EXIT
mkdir -p "$topdir"/{BUILD,BUILDROOT,RPMS,SOURCES,SPECS,SRPMS}

# Source0: exact-commit git archive; work only from its extraction.
archive="$topdir/SOURCES/voisu-${version}.tar.gz"
git archive --format=tar.gz --prefix="voisu-${version}/" "$commit" > "$archive"
extract_dir="$topdir/extract"
mkdir -p "$extract_dir"
tar -xzf "$archive" -C "$extract_dir"
src="$extract_dir/voisu-${version}"

# Source1: byte-reproducible vendor tarball + independent re-vendor self-test +
# source/ring-license sanity (shared helper).
commit_epoch=$(git show -s --format=%ct "$commit")
voisu_vendor_and_verify "$src" "$version" "$commit_epoch" "$topdir/scratch" \
    "$topdir/SOURCES/voisu-vendor-${version}.tar.gz"

# Spec from the extracted tree, Release/commit baked as %global (so the SRPM that
# -ba also emits is self-describing, matching build-srpm.sh).
{
    printf '%%global voisu_release %s\n' "$voisu_release"
    printf '%%global voisu_commit %s\n' "$commit"
    cat "$src/packaging/voisu.spec"
} > "$topdir/SPECS/voisu.spec"

rpmbuild -ba --define "_topdir $topdir" "$topdir/SPECS/voisu.spec"

rm -rf "$output_dir"
mkdir -p "$output_dir"
find "$topdir/RPMS" -type f -name '*.rpm' -exec cp -t "$output_dir" {} +
printf 'RPM artifacts (Release %s) written to %s\n' "$voisu_release" "$output_dir"
