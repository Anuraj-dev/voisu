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
# COPR wiring (HITL, see the PR body): configure a Custom-source package whose
# script clones this repo and execs this file, with
#   --script-builddeps "git cargo rust rust-std-static"
#   --script-resultdir _copr_srpm
#   --webhook-rebuild on
# A v* tag push then curls the project's custom webhook (see
# .github/workflows/copr-trigger.yml) and COPR rebuilds from HEAD.
#
# This script assumes it is executed from inside a checkout of the repo (the
# COPR Custom "script" field is a tiny bootstrap that git-clones then execs it).

root=$(git rev-parse --show-toplevel)
cd "$root"

version=0.1.0
commit=$(git rev-parse --verify HEAD)

# Monotonic snapshot Release, identical scheme to build-srpm.sh: leading 0.
# sorts before a tagged 1%{?dist}; commit count is the clock-skew-immune primary
# key. If HEAD is exactly on a v<version> tag, emit the tagged release N=1.
if tag_commit=$(git rev-parse --verify --quiet "refs/tags/v${version}^{commit}" 2>/dev/null) \
        && test "$tag_commit" = "$commit"; then
    voisu_release=1
else
    ct=$(git show -s --format=%ct "$commit")
    count=$(git rev-list --count "$commit")
    short=$(git rev-parse --short=12 "$commit")
    voisu_release="0.${count}.${ct}.git${short}"
fi

resultdir=${COPR_RESULTDIR:-"$root/_copr_srpm"}
rm -rf "$resultdir"
mkdir -p "$resultdir"

# Source0: exact-commit git archive.
git archive --format=tar.gz --prefix="voisu-${version}/" "$commit" \
    > "$resultdir/voisu-${version}.tar.gz"

# Source1: byte-reproducible vendor tarball (same invariants as build-srpm.sh).
commit_epoch=$(git show -s --format=%ct "$commit")
workdir=$(mktemp -d "${TMPDIR:-/var/tmp}/voisu-copr-vendor.XXXXXX")
trap 'rm -rf "$workdir"' EXIT
tar -xzf "$resultdir/voisu-${version}.tar.gz" -C "$workdir"
( cd "$workdir/voisu-${version}" \
    && cargo vendor --locked "$workdir/voisu-vendor-${version}" >/dev/null )
tar --sort=name --mtime="@${commit_epoch}" \
    --owner=0 --group=0 --numeric-owner --mode='u+rw,go=rX' \
    -C "$workdir" -cf - "voisu-vendor-${version}" | gzip -n \
    > "$resultdir/voisu-vendor-${version}.tar.gz"

# Spec: COPR runs a plain `rpmbuild -bs` with no --defines, so bake the computed
# Release and commit in as %global (equivalent to build-srpm.sh's --define).
{
    printf '%%global voisu_release %s\n' "$voisu_release"
    printf '%%global voisu_commit %s\n' "$commit"
    cat packaging/voisu.spec
} > "$resultdir/voisu.spec"

printf 'COPR sources (Release %s) written to %s\n' "$voisu_release" "$resultdir"
ls -1 "$resultdir"
