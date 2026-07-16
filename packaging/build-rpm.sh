#!/usr/bin/env bash
set -euo pipefail

root=$(git rev-parse --show-toplevel)
cd "$root"

requested_commit=${VOISU_COMMIT:-HEAD}
commit=$(git rev-parse --verify "${requested_commit}^{commit}")
version=0.1.0
output_dir=${VOISU_RPM_OUTPUT_DIR:-"$root/dist/rpm"}

if test -n "$(git status --porcelain)"; then
    printf '%s\n' 'refusing to package a dirty checkout; commit the tested tree first' >&2
    exit 1
fi
if test "$(git rev-parse HEAD)" != "$(git rev-parse "$commit")"; then
    printf '%s\n' 'VOISU_COMMIT must be the checked-out commit' >&2
    exit 1
fi

printf 'Testing exact Voisu commit: %s\n' "$commit"
cargo test --locked --workspace
cargo build --locked --release --workspace
cargo check --locked -p voisu-app --features overlay

topdir=$(mktemp -d "${TMPDIR:-/tmp}/voisu-rpmbuild.XXXXXX")
cleanup() { rm -rf "$topdir"; }
trap cleanup EXIT
mkdir -p "$topdir"/{BUILD,BUILDROOT,RPMS,SOURCES,SPECS,SRPMS}

archive="$topdir/SOURCES/voisu-${version}.tar.gz"
git archive --format=tar.gz --prefix="voisu-${version}/" "$commit" > "$archive"
tar -tzf "$archive" | grep -qx "voisu-${version}/Cargo.lock"
tar -tzf "$archive" | grep -qx "voisu-${version}/LICENSE"
tar -tzf "$archive" | grep -qx "voisu-${version}/packaging/voisu.service"

# Reproducibility: vendor from an extraction of the exact-commit git archive
# (never the working tree), then archive deterministically so the same commit
# always yields a byte-identical Source1 tarball. cargo vendor is a pure
# function of Cargo.lock; --sort/--owner/--group/--numeric-owner/--mtime plus
# `gzip -n` (no name/timestamp in the gzip header) remove every remaining
# source of nondeterminism.
commit_epoch=$(git show -s --format=%ct "$commit")
extract_dir="$topdir/extract"
mkdir -p "$extract_dir"
tar -xzf "$archive" -C "$extract_dir"
vendor_dir="$topdir/vendor/voisu-vendor-${version}"
mkdir -p "$topdir/vendor"
( cd "$extract_dir/voisu-${version}" && cargo vendor --locked "$vendor_dir" >/dev/null )

vendor_archive="$topdir/SOURCES/voisu-vendor-${version}.tar.gz"
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime="@${commit_epoch}" \
    -C "$topdir/vendor" -cf - "voisu-vendor-${version}" | gzip -n > "$vendor_archive"

# Self-test: re-archiving the same vendored tree must be byte-identical. This
# fails loudly if the deterministic tar/gzip invariants ever regress.
repro_archive="$topdir/voisu-vendor-repro.tar.gz"
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime="@${commit_epoch}" \
    -C "$topdir/vendor" -cf - "voisu-vendor-${version}" | gzip -n > "$repro_archive"
if ! cmp -s "$vendor_archive" "$repro_archive"; then
    printf '%s\n' 'vendor archive is not reproducible; deterministic tar invariants regressed' >&2
    exit 1
fi
rm -f "$repro_archive"

tar -tzf "$vendor_archive" > "$topdir/vendor-archive.list"
grep -q "^voisu-vendor-${version}/" "$topdir/vendor-archive.list"

cp packaging/voisu.spec "$topdir/SPECS/voisu.spec"

rpmbuild -ba \
    --define "_topdir $topdir" \
    --define "voisu_commit $commit" \
    "$topdir/SPECS/voisu.spec"

rm -rf "$output_dir"
mkdir -p "$output_dir"
find "$topdir/RPMS" -type f -name '*.rpm' -exec cp -t "$output_dir" {} +
printf 'RPM artifacts written to %s\n' "$output_dir"
