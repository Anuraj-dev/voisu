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
# queued jobs racing, a tag cut off a non-default branch). So this script never
# derives anything from mutable HEAD when releases exist: it fetches all tags +
# full history (FAIL-CLOSED — a failed fetch aborts rather than risking a stale
# or missing tag set), then builds the HIGHEST v<semver> tag's commit as the
# release, wherever HEAD points and whichever branch carried the tag. The tag
# name must match the version in that commit's own tree (catches a mis-cut tag).
# Only when NO release tag exists at all does it build a snapshot of HEAD — i.e.
# the pre-first-release bootstrap phase. After the first release, the COPR
# channel intentionally serves releases only; dev snapshots come from the local
# build-srpm.sh/build-rpm.sh paths. Queued-webhook races collapse harmlessly:
# whichever job runs later still builds the newest release. This pins provenance
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

# Fetch all tags + full history BEFORE classification. FAIL-CLOSED: if origin
# exists, a failed fetch aborts — building from potentially stale refs could
# silently publish a snapshot instead of a fresh release, or a stale tag after a
# respin retag. (No origin at all = deliberate local test mode, refs as-is.)
if git remote get-url origin >/dev/null 2>&1; then
    if ! git fetch --tags --force --prune; then
        printf '%s\n' 'refusing: tag fetch from origin failed; will not classify release vs snapshot from possibly stale refs' >&2
        exit 1
    fi
    if test "$(git rev-parse --is-shallow-repository)" = "true"; then
        if ! git fetch --unshallow --tags; then
            printf '%s\n' 'refusing: could not unshallow the clone; the pre-release commit count needs full history' >&2
            exit 1
        fi
    fi
else
    printf '%s\n' 'note: no origin remote; local test mode, using local refs as-is' >&2
fi
if test "$(git rev-parse --is-shallow-repository)" = "true"; then
    printf '%s\n' 'refusing: clone is still shallow; the pre-release commit count needs full history' >&2
    exit 1
fi

# Self-pin: the build target is the HIGHEST v<semver> release tag, independent of
# what HEAD points at. Only with no release tags at all (pre-first-release) does
# this build a snapshot of HEAD.
latest_tag=$(git tag --list 'v*' | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | sort -V | tail -n 1 || true)
if test -n "$latest_tag"; then
    build_commit=$(git rev-parse --verify "refs/tags/${latest_tag}^{commit}")
    tagged=yes
else
    build_commit=$(git rev-parse --verify HEAD)
    tagged=no
fi

# The version comes from the BUILD COMMIT's own tree (never the clone checkout):
# extract it first, derive, and for a release require the tag name to match.
probe_dir=$(mktemp -d "${TMPDIR:-/var/tmp}/voisu-copr-probe.XXXXXX")
trap 'rm -rf "$probe_dir"' EXIT
git archive --format=tar "$build_commit" | tar -x -C "$probe_dir"
version=$(voisu_derive_version "$probe_dir")
if test "$tagged" = yes && test "v${version}" != "$latest_tag"; then
    printf 'mis-cut tag: %s points at commit %s whose tree declares version %s\n' \
        "$latest_tag" "$build_commit" "$version" >&2
    exit 1
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
trap 'rm -rf "$workdir" "$probe_dir"' EXIT
tar -xzf "$resultdir/voisu-${version}.tar.gz" -C "$workdir"
src="$workdir/voisu-${version}"

# ($version already came from this same commit's tree via the probe extraction,
# and a release additionally proved v$version == the tag name — no re-check.)

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
