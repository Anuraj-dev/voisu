#!/usr/bin/env bash
set -euo pipefail

# Fedora latest install-smoke for the release gate (ticket 14).
#
# Builds the binary RPM with the existing tooling (packaging/build-rpm.sh) from
# the tag checkout, installs it via dnf, and asserts the binaries run and both
# shipped user units verify. Publishing is gated on this job, so it exercises
# the real Fedora package path end-to-end before the COPR channel rebuilds.
#
# NON-ROOT + %check: build-rpm.sh runs the workspace test suite (and rpmbuild's
# %check runs it again), so the build runs as a non-root `builder` user with
# TMPDIR=/var/tmp RUST_TEST_THREADS=4 (the tests spawn dbus-daemon/python/curl,
# and a size-capped /tmp tmpfs would overflow). This makes Fedora the long-pole
# smoke job. build-rpm.sh has no built-in %check switch, so the redundant test
# run is accepted rather than engineered around; see packaging/RELEASING.md.
#
# DEGRADED IN A CONTAINER: there is no systemd user session, so the full
# `voisu service install`/enable flow in packaging/fedora-smoke.sh cannot run
# here (that is the ticket 15 live-desktop smoke). Unit files are checked
# statically with `systemd-analyze verify`, which needs no session; the packaged
# metadata (Requires/file list) is asserted directly.

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$script_dir/../.." && pwd)

echo "== provision Fedora build + test toolchain =="
dnf -y install \
    rust cargo gcc make pkgconf-pkg-config \
    rpm-build systemd-rpm-macros systemd git \
    dbus-daemon python3 curl \
    gtk4-devel gtk4-layer-shell-devel libxkbcommon-devel

# makepkg-style hygiene: build the RPM (which runs tests) as a non-root user.
useradd -m builder 2>/dev/null || true
chown -R builder:builder "$repo_root"

echo "== build the RPM via packaging/build-rpm.sh (non-root, tests run) =="
su builder -c "
    set -euo pipefail
    cd '$repo_root'
    git config --global --add safe.directory '$repo_root'
    export TMPDIR=/var/tmp RUST_TEST_THREADS=4 VOISU_RPM_OUTPUT_DIR='$repo_root/dist/rpm'
    packaging/build-rpm.sh
"

# Install the main package + the Overlay subpackage (skip any debuginfo/debugsource).
echo "== dnf install the built RPMs =="
mapfile -t rpms < <(ls "$repo_root"/dist/rpm/voisu-*.x86_64.rpm | grep -vE 'debug(info|source)')
test "${#rpms[@]}" -ge 1
dnf -y install "${rpms[@]}"

echo "== binaries =="
rpm -q voisu
test -x /usr/bin/voisu
test -x /usr/bin/voisu-daemon
test -x /usr/bin/voisu-overlay
voisu --version
voisu-daemon --help >/dev/null

echo "== systemd-analyze verify (both user units) =="
systemd-analyze verify /usr/lib/systemd/user/voisu.service
systemd-analyze verify /usr/lib/systemd/user/voisu-overlay.service

echo "== fresh-home safety: the unit provisions its own config/state dirs =="
# A fresh home that never created ~/.config/voisu or ~/.local/state/voisu must
# still start. ProtectSystem=strict + ReadWritePaths=%h/.config/voisu failed
# systemd's namespace setup (status=226/NAMESPACE) on such a host, restart-looping
# into start-limit-hit. The unit now declares ConfigurationDirectory/StateDirectory
# (systemd creates them BEFORE sandbox setup); ReadWritePaths must reference no %h
# home path. No systemd user session exists in this container, so this static check
# is the strongest honest guard here; the live-desktop start smoke covers runtime.
unit=/usr/lib/systemd/user/voisu.service
grep -qx 'ConfigurationDirectory=voisu' "$unit"
grep -qx 'StateDirectory=voisu' "$unit"
if grep -Eq '^ReadWritePaths=.*%h' "$unit"; then
    echo "FAIL: voisu.service ReadWritePaths still references a %h home path"; exit 1
fi
echo "[evidence] voisu.service declares Configuration/StateDirectory and no %h ReadWritePaths"

# Packaged-metadata assertions (crib of packaging/fedora-smoke.sh's checks that
# do not need a user session).
echo "== packaged metadata =="
rpm_files=$(rpm -ql voisu)
grep -qx '/usr/bin/voisu' <<<"$rpm_files"
grep -qx '/usr/bin/voisu-daemon' <<<"$rpm_files"
grep -qx '/usr/lib/systemd/user/voisu.service' <<<"$rpm_files"
rpm_requires=$(rpm -q --requires voisu)
grep -qx 'wl-clipboard' <<<"$rpm_requires"
grep -qx 'wireplumber' <<<"$rpm_requires"
grep -qx 'curl' <<<"$rpm_requires"
grep -qx '/usr/bin/secret-tool' <<<"$rpm_requires"

echo "PASS: fedora:latest install-smoke (RPM build+install + binaries + unit verify + metadata)"
