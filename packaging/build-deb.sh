#!/usr/bin/env bash
set -euo pipefail

# Build the Voisu Debian/Ubuntu .deb via cargo-deb.
#
# Analogue of build-rpm.sh. Like the RPM, this refuses to package a dirty or
# non-HEAD checkout, builds the feature-gated Overlay binary with the same
# feature set the RPM uses (voisu-app/overlay), and produces a strictly
# increasing Debian version per distinct payload.
#
# MUST be run on Ubuntu (or any Debian derivative): the `$auto` dependency in
# crates/voisu-app/Cargo.toml is resolved by dpkg-shlibdeps, which reads the
# built ELF binaries and maps their linked SONAMEs + symbol version floors
# (GLIBC, GTK4, ...) to versioned Ubuntu package dependencies. dpkg-shlibdeps
# only exists on Debian derivatives and only resolves correctly when the
# binaries were compiled against that distribution's libraries, so the binaries
# must be BUILT here too, not cross-copied from another distro. CI builds this
# on Ubuntu 24.10 (ticket 14).
#
# The pure-validation guards below (dirty tree, non-HEAD, bad VOISU_DEB_RELEASE,
# missing/wrong cargo-deb, out-of-tree output dir) all run before any
# Debian-only step, so they are exercisable on any host.

root=$(realpath "$(git rev-parse --show-toplevel)")
cd "$root"

# Pinned to the exact cargo-deb version this package was validated with.
CARGO_DEB_VERSION=3.7.0

# --- reproducibility guards (mirror build-rpm.sh) --------------------------
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

# --- upstream version comes from the crate metadata, never hardcoded -------
base_version=$(cargo pkgid -p voisu-app | sed -E 's/.*[#@]//')
if ! printf '%s' "$base_version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+([.+~-][0-9A-Za-z.+~-]*)?$'; then
    printf 'could not determine a sane crate version from cargo pkgid (got "%s")\n' "$base_version" >&2
    exit 1
fi

# --- Debian version scheme -------------------------------------------------
# Every distinct payload gets a STRICTLY INCREASING version:
#   tagged release  -> <base>-N              (VOISU_DEB_RELEASE=N, N a positive integer)
#   dev build       -> <base>~git<count>.<ct>.<sha>-1
# The leading `~` guarantees any dev version sorts BEFORE the matching <base>-N
# release. For dev builds the PRIMARY ordering key is the commit count along
# history (strictly increasing for any descendant commit, immune to committer
# clock skew); the committer timestamp is a secondary tiebreaker only. Both are
# decimal integers compared numerically by dpkg. The short SHA is only an
# identifier appended last, never relied on for ordering. The count is only
# trustworthy with full history, so shallow clones are refused.
if test -n "${VOISU_DEB_RELEASE:-}"; then
    if ! printf '%s' "$VOISU_DEB_RELEASE" | grep -Eq '^[1-9][0-9]*$'; then
        printf 'VOISU_DEB_RELEASE must be a positive integer (got "%s")\n' "$VOISU_DEB_RELEASE" >&2
        exit 1
    fi
    # A tagged release must sit exactly on its release tag. Repo convention: a
    # `v`-prefixed semver tag (e.g. v0.1.0). No tags exist yet, so a release
    # build is only possible once the release commit is tagged.
    release_tag="v${base_version}"
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
    deb_version="${base_version}-${VOISU_DEB_RELEASE}"
else
    if test "$(git rev-parse --is-shallow-repository)" = "true"; then
        printf '%s\n' 'refusing a dev build from a shallow clone: the commit count that orders dev versions needs full history (git fetch --unshallow)' >&2
        exit 1
    fi
    ct=$(git show -s --format=%ct "$commit")
    count=$(git rev-list --count "$commit")
    short=$(git rev-parse --short=12 "$commit")
    deb_version="${base_version}~git${count}.${ct}.${short}-1"
fi

# --- toolchain checks ------------------------------------------------------
if ! command -v cargo-deb >/dev/null 2>&1; then
    printf 'cargo-deb not found; install it with: cargo install cargo-deb --version %s --locked\n' \
        "$CARGO_DEB_VERSION" >&2
    exit 1
fi
have_cargo_deb=$(cargo-deb --version 2>/dev/null | awk '{print $NF}')
if test "$have_cargo_deb" != "$CARGO_DEB_VERSION"; then
    printf 'cargo-deb %s is required but found "%s"; install it with: cargo install cargo-deb --version %s --locked --force\n' \
        "$CARGO_DEB_VERSION" "${have_cargo_deb:-none}" "$CARGO_DEB_VERSION" >&2
    exit 1
fi

# --- output dir: canonicalize and confine to $root/dist/ -------------------
# Guard BEFORE any destructive step. dist/ is git-ignored, so a clean checkout
# can still carry a symlinked dist redirecting the deletion boundary elsewhere
# ($root/dist -> $HOME); refuse that outright. $root is already canonical, so a
# non-symlink $root/dist canonicalizes to itself, and realpath -m on the output
# path then resolves any nested symlinks/.. -- only a target strictly beneath
# the real $root/dist/ is permitted; $root, $HOME, / are refused.
if test -L "$root/dist"; then
    printf 'refusing: %s/dist is a symlink; remove it so the output dir stays inside the tree\n' "$root" >&2
    exit 1
fi
dist_root="$root/dist"
output_dir=${VOISU_DEB_OUTPUT_DIR:-"$dist_root/deb"}
output_dir=$(realpath -m "$output_dir")
case "$output_dir" in
    "$dist_root"/?*) : ;;
    *) printf 'refusing to use output dir %s: must be under %s/\n' "$output_dir" "$dist_root" >&2
       exit 1 ;;
esac

# --- Debian-only step: $auto needs dpkg-shlibdeps --------------------------
if ! command -v dpkg-shlibdeps >/dev/null 2>&1; then
    printf '%s\n' \
        'dpkg-shlibdeps not found: this .deb must be built on Ubuntu/Debian.' \
        'cargo-deb'\''s `$auto` dependency discovery needs dpkg-dev (dpkg-shlibdeps)' \
        'to encode the real ELF/GLIBC library requirements, and the binaries must' \
        'be compiled against the target distribution. Build on Ubuntu 24.10 (or in' \
        'CI, ticket 14): apt-get install dpkg-dev, then re-run this script.' >&2
    exit 1
fi

# --- generate a changelog whose top entry matches the computed version -----
# git formats the date (correct RFC-2822 weekday), keeping lintian happy.
changelog_date=$(git show -s --format=%cd --date=format:'%a, %d %b %Y %H:%M:%S %z' "$commit")
short=$(git rev-parse --short=12 "$commit")
mkdir -p "$root/target/deb"
cat > "$root/target/deb/changelog" <<EOF
voisu (${deb_version}) unstable; urgency=medium

  * Automated Voisu package build of commit ${short}.
  * Ships voisu, voisu-daemon and voisu-overlay plus both systemd user units,
    mirroring the Fedora RPM layout and enable-instruction UX.

 -- Anuraj Jit Saikia <rajasaikia1644@gmail.com>  ${changelog_date}
EOF

# --- build (same feature set as the RPM) -----------------------------------
cargo build --locked --release --workspace
cargo build --locked --release -p voisu-app --features overlay --bin voisu-overlay

# --- recreate the (already-validated) output dir ---------------------------
rm -rf "$output_dir"
mkdir -p "$output_dir"

# cargo-deb builds voisu-app with the overlay feature, strips the binaries,
# resolves `$auto` via dpkg-shlibdeps, and writes the versioned .deb.
cargo deb \
    -p voisu-app \
    --features overlay \
    --deb-version "$deb_version" \
    --output "$output_dir" \
    -- --locked

printf 'deb artifacts (version %s) written to %s\n' "$deb_version" "$output_dir"
ls -1 "$output_dir"/*.deb
