#!/usr/bin/env bash
set -euo pipefail

# COPR "Custom" source-method script for the Voisu COPR channel (ticket 12,
# GH issue #44).
#
# COPR's SRPM-generation phase HAS network (unlike the mock RPM phase), so this
# script vendors the whole crate graph here, at SRPM time, and deposits a
# specfile + the two source tarballs into a result directory. COPR then
# assembles the .src.rpm from them (a Custom script outputs sources, NOT an
# SRPM). The vendor tarball rides inside that SRPM as Source1, so the offline
# mock RPM build consumes it with no crates.io access. See build-srpm.sh for the
# equivalent local path that runs `rpmbuild -bs` directly.
#
# SELF-PINNING PROVENANCE. The webhook (.github/workflows/copr-trigger.yml) only
# pokes COPR to "rebuild the package"; it carries no commit. COPR then clones the
# repo, but a plain HEAD clone is a moving target (tag pushed before the branch,
# queued jobs racing, a tag cut off a non-default branch). So this script does
# NOT trust clone HEAD for releases: it fetches all tags + full history, derives
# the version from the exact-commit cargo metadata, and if tag v<version> exists
# it builds the RELEASE from THAT tag's commit regardless of what HEAD points at.
# With no matching tag it builds a snapshot of HEAD. This pins release provenance
# using only the COPR_WEBHOOK_URL secret we have; a token-based pinned-ref API
# build (copr-cli against an explicit commit) needs a new COPR API secret and is
# ticket 14's option (HITL), not wired here.
#
# The computed Release/commit are baked into the deposited spec as %global
# because COPR runs a plain `rpmbuild -bs` with no --defines; the %global lines
# make the SRPM self-describing and its mock rebuilds NEVR-stable. Shared logic
# (version derivation, unified Release policy, vendor+verify, path confinement)
# lives in packaging/rpm-lib.sh so this path runs the SAME checks as build-srpm.sh.
#
# Respin of a tagged release (0.1.0-2 without new code): bump packaging/rpm-release
# and move the v<version> tag onto the commit that carries the bump (retag), then
# re-trigger the webhook.
#
# Executed from inside a checkout of the repo (the COPR Custom "script" field is a
# tiny bootstrap that git-clones this repo and execs this file).

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=packaging/rpm-lib.sh
source "$script_dir/../rpm-lib.sh"

for tool in git cargo tar gzip; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        printf '%s not found; add it to the COPR Custom --script-builddeps\n' "$tool" >&2
        exit 1
    fi
done

root=$(git rev-parse --show-toplevel)
cd "$root"

# Fetch all tags + full history BEFORE classification, and reject a clone that is
# still shallow (the commit count that orders pre-release builds needs it).
git fetch --tags --force --prune >/dev/null 2>&1 || true
if test "$(git rev-parse --is-shallow-repository)" = "true"; then
    git fetch --unshallow >/dev/null 2>&1 || true
fi
if test "$(git rev-parse --is-shallow-repository)" = "true"; then
    printf '%s\n' 'refusing: clone is still shallow after fetch; the pre-release commit count needs full history' >&2
    exit 1
fi

# Version from the current checkout, verified against its spec.
version=$(voisu_derive_version "$root")

# Self-pin: build the release from tag v<version> if it exists, else snapshot HEAD.
if tag_commit=$(git rev-parse --verify --quiet "refs/tags/v${version}^{commit}" 2>/dev/null); then
    build_commit=$tag_commit
    tagged=yes
else
    build_commit=$(git rev-parse --verify HEAD)
    tagged=no
fi

# Result dir: confine strictly under the checkout (rm -rf target). Configure the
# COPR Custom package with --script-resultdir _copr_srpm to match.
resultdir=$(voisu_confine_under "$root" "$root/_copr_srpm")
rm -rf "$resultdir"
mkdir -p "$resultdir"

# Source0: exact (build_commit) git archive; work only from its extraction.
git archive --format=tar.gz --prefix="voisu-${version}/" "$build_commit" \
    > "$resultdir/voisu-${version}.tar.gz"
workdir=$(mktemp -d "${TMPDIR:-/var/tmp}/voisu-copr.XXXXXX")
trap 'rm -rf "$workdir"' EXIT
tar -xzf "$resultdir/voisu-${version}.tar.gz" -C "$workdir"
src="$workdir/voisu-${version}"

# The built commit's tree must agree on the version (tag v<version> guarantees it;
# this catches a mis-cut tag).
built_version=$(voisu_derive_version "$src")
if test "$built_version" != "$version"; then
    printf 'version mismatch: checkout says %s but build commit %s says %s\n' \
        "$version" "$build_commit" "$built_version" >&2
    exit 1
fi

# Unified Release policy (packaging/rpm-lib.sh).
if test "$tagged" = yes; then
    tag_number=$(voisu_tag_release_number "$src")
    voisu_release=$(voisu_compute_release "$root" "$build_commit" yes "$tag_number")
else
    voisu_release=$(voisu_compute_release "$root" "$build_commit" no "")
fi
voisu_assert_release_ordering "$version"

# Source1: byte-reproducible vendor tarball + independent re-vendor self-test +
# source/ring-license sanity (the SAME checks build-srpm.sh runs).
commit_epoch=$(git show -s --format=%ct "$build_commit")
voisu_vendor_and_verify "$src" "$version" "$commit_epoch" "$workdir/scratch" \
    "$resultdir/voisu-vendor-${version}.tar.gz"

# Spec from the extracted (built) tree, Release/commit baked as %global.
{
    printf '%%global voisu_release %s\n' "$voisu_release"
    printf '%%global voisu_commit %s\n' "$build_commit"
    cat "$src/packaging/voisu.spec"
} > "$resultdir/voisu.spec"

printf 'COPR sources (Release %s, commit %s) written to %s\n' \
    "$voisu_release" "$build_commit" "$resultdir"
ls -1 "$resultdir"
