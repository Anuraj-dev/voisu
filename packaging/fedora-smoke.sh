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
artifact_name=$(rpm -qp --qf '%{NAME}' "$rpm_path")
test "$artifact_name" = voisu
expected_nevra=$(rpm -qp --qf '%{NAME}-%{VERSION}-%{RELEASE}.%{ARCH}' "$rpm_path")
installed_nevra=$(rpm -q --qf '%{NAME}-%{VERSION}-%{RELEASE}.%{ARCH}' voisu 2>/dev/null || true)
installed_before=0
if test -n "$installed_nevra"; then
    installed_before=1
    if test "$installed_nevra" != "$expected_nevra"; then
        printf 'refusing to clobber installed Voisu %s with %s\n' "$installed_nevra" "$expected_nevra" >&2
        exit 1
    fi
fi
for path in /usr/bin/voisu /usr/bin/voisu-daemon /usr/lib/systemd/user/voisu.service; do
    if test -e "$path" || test -L "$path"; then
        if test "$installed_before" -ne 1; then
            printf 'refusing to clobber pre-existing Voisu path %s not owned by the exact RPM\n' "$path" >&2
            exit 1
        fi
    fi
done

if test "$(id -u)" -eq 0; then
    dnf_cmd=(dnf)
else
    dnf_cmd=(sudo dnf)
fi
payload_dir=
cleanup() {
    rc=$?
    if test -n "${payload_dir:-}"; then
        rm -rf "$payload_dir"
    fi
    if test "$installed_before" -eq 0; then
        if test -x /usr/bin/voisu; then
            /usr/bin/voisu service uninstall >/dev/null 2>&1 || true
        fi
        current_nevra=$(rpm -q --qf '%{NAME}-%{VERSION}-%{RELEASE}.%{ARCH}' voisu 2>/dev/null || true)
        if test -n "$current_nevra"; then
            "${dnf_cmd[@]}" remove -y voisu >/dev/null 2>&1 || true
        fi
    fi
    exit "$rc"
}
trap cleanup EXIT

"${dnf_cmd[@]}" install -y "$rpm_path"
installed_nevra=$(rpm -q --qf '%{NAME}-%{VERSION}-%{RELEASE}.%{ARCH}' voisu)
test "$installed_nevra" = "$expected_nevra"
rpm -V voisu

payload_dir=$(mktemp -d "${TMPDIR:-/tmp}/voisu-rpm-payload.XXXXXX")
rpm2cpio "$rpm_path" | (cd "$payload_dir" && cpio -idm --quiet)
for path in /usr/bin/voisu /usr/bin/voisu-daemon; do
    expected_sha=$(sha256sum "$payload_dir$path" | cut -d' ' -f1)
    installed_sha=$(sha256sum "$path" | cut -d' ' -f1)
    test "$installed_sha" = "$expected_sha"
done
rm -rf "$payload_dir"
payload_dir=

if test -n "$expected_commit"; then
    release=$(rpm -qp --qf '%{RELEASE}\n' "$rpm_path")
    expected_release_commit=${expected_commit:0:12}
    case "$release" in
        *".git${expected_release_commit}"*) ;;
        *) printf 'RPM Release %s does not contain tested commit %s\n' "$release" "$expected_commit" >&2; exit 1 ;;
    esac
fi
rpm -qpl "$rpm_path" | grep -qx '/usr/bin/voisu'
rpm -qpl "$rpm_path" | grep -qx '/usr/bin/voisu-daemon'
rpm -qpl "$rpm_path" | grep -qx '/usr/lib/systemd/user/voisu.service'
rpm -qp --requires "$rpm_path" | grep -qx 'wl-clipboard'
rpm -qp --requires "$rpm_path" | grep -qx 'pipewire-utils'
rpm -qp --requires "$rpm_path" | grep -qx 'wireplumber'
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
# XDG-user-data shadow. Cleanup removes only the artifact installed by this
# harness; pre-existing exact-NEVRA installs are left untouched.
/usr/bin/voisu service install
/usr/bin/voisu service status || test "$?" -eq 3

if test "${VOISU_FEDORA_LIVE_SMOKE:-0}" != 1; then
    printf '%s\n' 'packaged artifact smoke passed; set VOISU_FEDORA_LIVE_SMOKE=1 for the desktop Recording smoke'
    exit 0
fi

/usr/bin/voisu service start
/usr/bin/voisu service status
/usr/bin/voisu doctor
/usr/bin/voisu start
sleep "${VOISU_RECORDING_SECONDS:-3}"
/usr/bin/voisu stop
test -n "$(wl-paste --no-newline)"
/usr/bin/voisu service status
