#!/usr/bin/env bash
set -euo pipefail

rpm_path=${1:-}
expected_commit=${2:-}
if test -z "$rpm_path"; then
    printf 'usage: %s /path/to/voisu-*.rpm [tested-commit]\n' "$0" >&2
    exit 2
fi

test -r "$rpm_path"
rpm -qip "$rpm_path" >/dev/null
if test -n "$expected_commit"; then
    release=$(rpm -qp --qf '%{RELEASE}\n' "$rpm_path")
    case "$release" in
        *".git${expected_commit}"*) ;;
        *) printf 'RPM Release %s does not contain tested commit %s\n' "$release" "$expected_commit" >&2; exit 1 ;;
    esac
fi
rpm -qpl "$rpm_path" | grep -qx '/usr/bin/voisu'
rpm -qpl "$rpm_path" | grep -qx '/usr/bin/voisu-daemon'
rpm -qpl "$rpm_path" | grep -qx '/usr/lib/systemd/user/voisu.service'
rpm -qp --requires "$rpm_path" | grep -qx 'wl-clipboard'
rpm -qp --requires "$rpm_path" | grep -qx 'pipewire-utils'
rpm -qp --requires "$rpm_path" | grep -qx 'curl'
rpm -qp --requires "$rpm_path" | grep -qx 'libsecret'
rpm -qp --recommends "$rpm_path" | grep -q '^libei'

test -x /usr/bin/voisu
test -x /usr/bin/voisu-daemon
test -r /usr/lib/systemd/user/voisu.service
grep -qx 'ExecStart=/usr/bin/voisu-daemon --systemd' /usr/lib/systemd/user/voisu.service
/usr/bin/voisu --help >/dev/null
systemctl --user daemon-reload

# This exercises the packaged-unit preference and removes any old Voisu
# XDG-user-data shadow. It does not remove credentials, state, or diagnostics.
/usr/bin/voisu service install
/usr/bin/voisu service status || test "$?" -eq 3

if test "${VOISU_FEDORA_LIVE_SMOKE:-0}" != 1; then
    printf '%s\n' 'packaged artifact smoke passed; set VOISU_FEDORA_LIVE_SMOKE=1 for the desktop Recording smoke'
    exit 0
fi

/usr/bin/voisu doctor
/usr/bin/voisu service start
/usr/bin/voisu start
sleep "${VOISU_RECORDING_SECONDS:-3}"
/usr/bin/voisu stop
test -n "$(wl-paste --no-newline)"
/usr/bin/voisu service status
