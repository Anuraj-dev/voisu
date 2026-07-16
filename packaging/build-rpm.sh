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
# List once to a file: `tar -tzf | grep -q` dies of SIGPIPE under pipefail
# when grep exits on an early match while tar is still writing.
tar -tzf "$archive" > "$topdir/source-archive.list"
grep -qx "voisu-${version}/Cargo.lock" "$topdir/source-archive.list"
grep -qx "voisu-${version}/LICENSE" "$topdir/source-archive.list"
grep -qx "voisu-${version}/packaging/voisu.service" "$topdir/source-archive.list"

# Reproducibility: vendor from an extraction of the exact-commit git archive
# (never the working tree), and archive deterministically. --sort fixes entry
# order, --owner/--group/--numeric-owner fix ownership, --mtime fixes timestamps,
# --mode normalizes permission bits (so a differing build umask cannot change the
# headers), and `gzip -n` drops the gzip name/timestamp. The self-test then runs
# an INDEPENDENT cargo vendor of the same commit and requires a byte-identical
# archive, proving cargo vendor output stability rather than only tar/gzip
# determinism.
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
    # $1 = parent directory containing voisu-vendor-${version}, $2 = output path
    tar --sort=name --mtime="@${commit_epoch}" \
        --owner=0 --group=0 --numeric-owner --mode='u+rw,go=rX' \
        -C "$1" -cf - "voisu-vendor-${version}" | gzip -n > "$2"
}

vendor_into "$topdir/vendor"
vendor_archive="$topdir/SOURCES/voisu-vendor-${version}.tar.gz"
deterministic_vendor_archive "$topdir/vendor" "$vendor_archive"

# Discriminating self-test: an independent cargo vendor of the same commit must
# produce a byte-identical archive. This fails loudly on either cargo vendor
# instability or a regression in the deterministic tar/gzip invariants.
vendor_into "$topdir/vendor-verify"
repro_archive="$topdir/voisu-vendor-repro.tar.gz"
deterministic_vendor_archive "$topdir/vendor-verify" "$repro_archive"
if ! cmp -s "$vendor_archive" "$repro_archive"; then
    printf '%s\n' 'vendor archive is not reproducible: an independent cargo vendor of the same commit differs' >&2
    exit 1
fi
rm -rf "$topdir/vendor-verify" "$repro_archive"

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
