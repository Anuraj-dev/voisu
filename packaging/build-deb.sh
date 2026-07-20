#!/usr/bin/env bash
set -euo pipefail

# Build the Voisu Debian/Ubuntu .deb via cargo-deb.
#
# Analogue of build-rpm.sh. Like the RPM %build, the Overlay binary is
# feature-gated, so the build enables the exact same feature set the RPM uses
# (voisu-app/overlay) so voisu-overlay is produced and shipped alongside the
# base binaries. cargo-deb reads [package.metadata.deb] from
# crates/voisu-app/Cargo.toml.

root=$(git rev-parse --show-toplevel)
cd "$root"

output_dir=${VOISU_DEB_OUTPUT_DIR:-"$root/dist/deb"}

if ! command -v cargo-deb >/dev/null 2>&1; then
    printf '%s\n' 'cargo-deb not found; install it with: cargo install cargo-deb' >&2
    exit 1
fi

# Same feature set as the RPM (see packaging/voisu.spec %build and
# packaging/build-rpm.sh): build the workspace, then ensure the feature-gated
# Overlay binary is built with the overlay feature.
cargo build --locked --release --workspace
cargo build --locked --release -p voisu-app --features overlay --bin voisu-overlay

mkdir -p "$output_dir"
# cargo-deb builds voisu-app with the overlay feature (same feature set as the
# RPM), then copies and strips the three binaries named in the asset list and
# assembles the .deb. The build above primed target/release so this is
# incremental. `-- --locked` forwards to the underlying cargo build.
cargo deb \
    -p voisu-app \
    --features overlay \
    --output "$output_dir" \
    -- --locked

printf 'deb artifacts written to %s\n' "$output_dir"
ls -1 "$output_dir"/*.deb
