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

root=$(git rev-parse --show-toplevel)
cd "$root"

# Pinned to the version validated for this package.
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

# --- toolchain check -------------------------------------------------------
if ! command -v cargo-deb >/dev/null 2>&1; then
    printf 'cargo-deb not found; install it with: cargo install cargo-deb --version %s --locked\n' \
        "$CARGO_DEB_VERSION" >&2
    exit 1
fi
if ! command -v dpkg-shlibdeps >/dev/null 2>&1; then
    printf '%s\n' \
        'dpkg-shlibdeps not found: this .deb must be built on Ubuntu/Debian.' \
        'cargo-deb'\''s `$auto` dependency discovery needs dpkg-dev (dpkg-shlibdeps)' \
        'to encode the real ELF/GLIBC library requirements, and the binaries must' \
        'be compiled against the target distribution. Build on Ubuntu 24.10 (or in' \
        'CI, ticket 14): apt-get install dpkg-dev, then re-run this script.' >&2
    exit 1
fi

# --- Debian version scheme -------------------------------------------------
# Every distinct payload gets a strictly increasing version:
#   tagged release  -> 0.1.0-N          (VOISU_DEB_RELEASE=N)
#   dev build       -> 0.1.0~gitYYYYMMDD.<12-char sha>-1
# The `~` makes a dev version sort BEFORE the matching 0.1.0-N release.
base_version=0.1.0
short=$(git rev-parse --short=12 "$commit")
if test -n "${VOISU_DEB_RELEASE:-}"; then
    deb_version="${base_version}-${VOISU_DEB_RELEASE}"
else
    day=$(git show -s --format=%cd --date=format:%Y%m%d "$commit")
    deb_version="${base_version}~git${day}.${short}-1"
fi

# --- generate a changelog whose top entry matches the computed version -----
# git formats the date (correct RFC-2822 weekday), keeping lintian happy.
changelog_date=$(git show -s --format=%cd --date=format:'%a, %d %b %Y %H:%M:%S %z' "$commit")
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

# --- clean output dir (guard against empty/root path) ----------------------
output_dir=${VOISU_DEB_OUTPUT_DIR:-"$root/dist/deb"}
case "$output_dir" in
    ""|/) printf 'refusing to operate on output dir "%s"\n' "$output_dir" >&2; exit 1 ;;
esac
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
