#!/usr/bin/env bash
set -euo pipefail

# Fedora packaged-artifact smoke harness.
#
# Artifact binding: the smoke must exercise the exact supplied RPM, never
# whatever happens to be installed. `dnf install` will not replace a package
# with the same NEVRA, so a supplied RPM that differs only in payload (for
# example a swapped voisu.service) would otherwise slip through while `rpm -V`
# only checks the installed package against its own database. To prevent that,
# after ensuring the package is installed we compare the full file manifest of
# the supplied RPM (`rpm -qp --dump`: path, size, mtime, digest, mode, owner,
# group) against the installed package (`rpm -q --dump`) and abort on any
# mismatch. When a same-NEVRA package is already installed and its payload
# differs from the supplied RPM, the harness refuses rather than silently
# reinstalling.
#
# User-service state: `voisu service install` (and, in the live smoke,
# `voisu service start`) enable the user unit, may restart it, and migrate away
# any Ticket 09 XDG user-data shadow. The cleanup trap runs on success and on
# failure and restores the mutated user-service state: it stops and disables the
# unit the smoke enabled, restores any Ticket 09 XDG shadow unit/daemon it
# backed up before mutating, and returns enablement/active state to the values
# observed before the smoke. It does not attempt to restore unrelated drop-ins
# or non-Voisu user state.

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

xdg_config=${XDG_CONFIG_HOME:-$HOME/.config}
xdg_data=${XDG_DATA_HOME:-$HOME/.local/share}
shadow_unit="$xdg_config/systemd/user/voisu.service"
shadow_daemon="$xdg_data/voisu/bin/voisu-daemon"

restore_user_service() {
    # Undo the user-service mutations from `voisu service install`/`start`:
    # stop and disable what the smoke enabled, restore any Ticket 09 XDG shadow
    # it migrated away, and return enablement/active state to the pre-smoke
    # values captured in the snapshot.
    if test -z "${snapshot_dir:-}"; then
        return
    fi
    systemctl --user stop voisu.service >/dev/null 2>&1 || true
    systemctl --user disable voisu.service >/dev/null 2>&1 || true
    if test -f "$snapshot_dir/voisu.service"; then
        mkdir -p "$(dirname "$shadow_unit")"
        cp -p "$snapshot_dir/voisu.service" "$shadow_unit"
    fi
    if test -f "$snapshot_dir/voisu-daemon"; then
        mkdir -p "$(dirname "$shadow_daemon")"
        cp -p "$snapshot_dir/voisu-daemon" "$shadow_daemon"
    fi
    systemctl --user daemon-reload >/dev/null 2>&1 || true
    if test "${pre_enabled:-}" = enabled; then
        systemctl --user enable voisu.service >/dev/null 2>&1 || true
    fi
    if test "${pre_active:-}" = active; then
        systemctl --user start voisu.service >/dev/null 2>&1 || true
    fi
    rm -rf "$snapshot_dir"
    snapshot_dir=
}

cleanup() {
    rc=$?
    if test "$installed_before" -eq 0; then
        if test -x /usr/bin/voisu; then
            /usr/bin/voisu service uninstall >/dev/null 2>&1 || true
        fi
        current_nevra=$(rpm -q --qf '%{NAME}-%{VERSION}-%{RELEASE}.%{ARCH}' voisu 2>/dev/null || true)
        if test -n "$current_nevra"; then
            "${dnf_cmd[@]}" remove -y voisu >/dev/null 2>&1 || true
        fi
    fi
    restore_user_service
    exit "$rc"
}
trap cleanup EXIT

"${dnf_cmd[@]}" install -y "$rpm_path"
installed_nevra=$(rpm -q --qf '%{NAME}-%{VERSION}-%{RELEASE}.%{ARCH}' voisu)
test "$installed_nevra" = "$expected_nevra"

# Bind the installed package to the supplied RPM's actual payload. A pre-existing
# same-NEVRA install that dnf declined to replace is caught here because its
# manifest will not match the supplied RPM.
supplied_dump=$(rpm -qp --dump "$rpm_path" | sort)
installed_dump=$(rpm -q --dump voisu | sort)
if test "$supplied_dump" != "$installed_dump"; then
    printf 'installed Voisu payload does not match the supplied RPM %s; refusing to smoke the wrong artifact\n' "$rpm_path" >&2
    exit 1
fi
rpm -V voisu

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

# Snapshot the user-service state that `voisu service install`/`start` mutate so
# the cleanup trap can restore it on both success and failure.
snapshot_dir=$(mktemp -d "${TMPDIR:-/tmp}/voisu-smoke-snapshot.XXXXXX")
pre_enabled=$(systemctl --user is-enabled voisu.service 2>/dev/null || true)
pre_active=$(systemctl --user is-active voisu.service 2>/dev/null || true)
if test -f "$shadow_unit"; then
    cp -p "$shadow_unit" "$snapshot_dir/voisu.service"
fi
if test -f "$shadow_daemon"; then
    cp -p "$shadow_daemon" "$snapshot_dir/voisu-daemon"
fi

# This exercises the packaged-unit preference and removes any old Voisu
# XDG-user-data shadow. The snapshot above lets cleanup restore the mutated
# user-service state; RPM-owned files are never modified by the smoke.
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
