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
# `voisu service start`) enable the daemon unit, may restart it, and migrate away
# any Ticket 09 XDG user-data shadow. When the optional Overlay RPM is already
# installed, service install also enables and starts its independent user unit.
# The cleanup trap runs on success and on
# failure and restores the mutated user-service state: it restores any Ticket 09
# XDG shadow unit/daemon it backed up before mutating, and — whenever a unit
# exists again after restoration (the package was already installed before the
# smoke, or the Ticket 09 shadow was put back) — restores the unit's prior
# enablement (including enabled-runtime) and active state. Restoration is judged on the END
# STATE, not on individual step exit codes: after restoring, the harness compares
# systemd's reported enablement/active state against the snapshot (and, for a
# fresh install, verifies the smoke-installed RPM is gone and the unit is not
# left enabled); any mismatch is printed and forces a non-zero exit even when the
# smoke itself passed. Enablement states other than
# enabled/enabled-runtime/disabled cannot be faithfully reproduced and are
# reported rather than silently downgraded. Unrelated drop-ins and non-Voisu user
# state are out of scope.

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
# `rpm -q` prints "package voisu is not installed" to STDOUT when absent, so
# the query result is only meaningful when the query itself succeeds.
installed_before=0
if installed_nevra=$(rpm -q --qf '%{NAME}-%{VERSION}-%{RELEASE}.%{ARCH}' voisu 2>/dev/null); then
    installed_before=1
    if test "$installed_nevra" != "$expected_nevra"; then
        printf 'refusing to clobber installed Voisu %s with %s\n' "$installed_nevra" "$expected_nevra" >&2
        exit 1
    fi
fi
overlay_installed_before=0
if rpm -q voisu-overlay >/dev/null 2>&1; then
    overlay_installed_before=1
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
    # Undo the user-service mutations from `voisu service install`/`start` and
    # return 0 only if every promised restoration succeeded. Restoration failures
    # are printed individually; the caller forces a non-zero exit so a "success"
    # smoke can never silently leave the service disabled/stopped.
    if test -z "${snapshot_dir:-}"; then
        return 0
    fi
    local failed=0
    local unit_restored=0
    # Best-effort quiesce whatever the smoke enabled before restoring.
    systemctl --user stop voisu.service >/dev/null 2>&1 || true
    systemctl --user disable voisu.service >/dev/null 2>&1 || true
    if test "$overlay_installed_before" -eq 1; then
        systemctl --user stop voisu-overlay.service >/dev/null 2>&1 || true
        systemctl --user disable voisu-overlay.service >/dev/null 2>&1 || true
    fi
    # Restore any Ticket 09 XDG shadow the smoke migrated away (user data,
    # independent of the RPM).
    if test -f "$snapshot_dir/voisu.service"; then
        if { mkdir -p "$(dirname "$shadow_unit")" \
            && cp -p "$snapshot_dir/voisu.service" "$shadow_unit"; }; then
            unit_restored=1
        else
            printf 'restore: could not restore Ticket 09 unit %s\n' "$shadow_unit" >&2
            failed=1
        fi
    fi
    if test -f "$snapshot_dir/voisu-daemon"; then
        if ! { mkdir -p "$(dirname "$shadow_daemon")" \
            && cp -p "$snapshot_dir/voisu-daemon" "$shadow_daemon"; }; then
            printf 'restore: could not restore Ticket 09 daemon %s\n' "$shadow_daemon" >&2
            failed=1
        fi
    fi
    systemctl --user daemon-reload >/dev/null 2>&1 \
        || { printf 'restore: daemon-reload failed\n' >&2; failed=1; }
    # The unit's prior enablement/active state can be faithfully restored when a
    # unit exists again after restoration: either the pre-existing package is
    # left installed, or the Ticket 09 XDG shadow was just put back (a fresh
    # install can have had an active Ticket 09 service before the smoke).
    if test "$installed_before" -eq 1 || test "$unit_restored" -eq 1; then
        expected_enabled=${pre_enabled:-disabled}
        case "$expected_enabled" in
            enabled)
                systemctl --user enable voisu.service >/dev/null 2>&1 || true
                ;;
            enabled-runtime)
                systemctl --user enable --runtime voisu.service >/dev/null 2>&1 || true
                ;;
            disabled)
                : # pre-smoke state was not enabled; leaving it disabled is faithful.
                ;;
            *)
                printf 'restore: cannot faithfully restore enablement state "%s"; left disabled\n' "$pre_enabled" >&2
                failed=1
                expected_enabled=
                ;;
        esac
        if test "${pre_active:-}" = active; then
            systemctl --user start voisu.service >/dev/null 2>&1 || true
        fi
        # Restoration is judged on the END STATE, never on individual step exit
        # codes: compare what systemd now reports against the snapshot.
        final_enabled=$(systemctl --user is-enabled voisu.service 2>/dev/null || true)
        final_enabled=${final_enabled:-disabled}
        if test -n "$expected_enabled" && test "$final_enabled" != "$expected_enabled"; then
            printf 'restore: enablement is "%s" but was "%s" before the smoke\n' \
                "$final_enabled" "$expected_enabled" >&2
            failed=1
        fi
        final_active=$(systemctl --user is-active voisu.service 2>/dev/null || true)
        if test "${pre_active:-}" = active && test "$final_active" != active; then
            printf 'restore: voisu.service is "%s" but was active before the smoke\n' "$final_active" >&2
            failed=1
        fi
        if test "${pre_active:-}" != active && test "$final_active" = active; then
            printf 'restore: voisu.service left active but was "%s" before the smoke\n' \
                "${pre_active:-inactive}" >&2
            failed=1
        fi
    fi
    if test "$overlay_installed_before" -eq 1; then
        expected_overlay_enabled=${pre_overlay_enabled:-disabled}
        case "$expected_overlay_enabled" in
            enabled)
                systemctl --user enable voisu-overlay.service >/dev/null 2>&1 || true
                ;;
            enabled-runtime)
                systemctl --user enable --runtime voisu-overlay.service >/dev/null 2>&1 || true
                ;;
            disabled)
                :
                ;;
            *)
                printf 'restore: cannot faithfully restore Overlay enablement state "%s"; left disabled\n' \
                    "$pre_overlay_enabled" >&2
                failed=1
                expected_overlay_enabled=
                ;;
        esac
        if test "${pre_overlay_active:-}" = active; then
            systemctl --user start voisu-overlay.service >/dev/null 2>&1 || true
        fi
        final_overlay_enabled=$(systemctl --user is-enabled voisu-overlay.service 2>/dev/null || true)
        final_overlay_enabled=${final_overlay_enabled:-disabled}
        if test -n "$expected_overlay_enabled" \
            && test "$final_overlay_enabled" != "$expected_overlay_enabled"; then
            printf 'restore: voisu-overlay.service enablement is "%s" but was "%s" before the smoke\n' \
                "$final_overlay_enabled" "$expected_overlay_enabled" >&2
            failed=1
        fi
        final_overlay_active=$(systemctl --user is-active voisu-overlay.service 2>/dev/null || true)
        if test "${pre_overlay_active:-}" = active && test "$final_overlay_active" != active; then
            printf 'restore: voisu-overlay.service is "%s" but was active before the smoke\n' \
                "$final_overlay_active" >&2
            failed=1
        fi
        if test "${pre_overlay_active:-}" != active && test "$final_overlay_active" = active; then
            printf 'restore: voisu-overlay.service left active but was "%s" before the smoke\n' \
                "${pre_overlay_active:-inactive}" >&2
            failed=1
        fi
    fi
    rm -rf "$snapshot_dir"
    snapshot_dir=
    return "$failed"
}

cleanup() {
    rc=$?
    if test "$installed_before" -eq 0; then
        if test -x /usr/bin/voisu; then
            /usr/bin/voisu service uninstall >/dev/null 2>&1 \
                || printf 'cleanup: voisu service uninstall failed\n' >&2
        fi
        if rpm -q voisu >/dev/null 2>&1; then
            "${dnf_cmd[@]}" remove -y voisu >/dev/null 2>&1 || true
        fi
        # Judged on the end state: the smoke-installed RPM must be gone, the
        # unit must not be left enabled, and the smoke-started service must not
        # still be running; any leftover forces a failure exit.
        if rpm -q voisu >/dev/null 2>&1; then
            printf 'cleanup: smoke-installed Voisu RPM is still installed\n' >&2
            if test "$rc" -eq 0; then rc=1; fi
        fi
        leftover_enabled=$(systemctl --user is-enabled voisu.service 2>/dev/null || true)
        case "$leftover_enabled" in
            enabled|enabled-runtime)
                printf 'cleanup: voisu.service left %s after removal\n' "$leftover_enabled" >&2
                if test "$rc" -eq 0; then rc=1; fi
                ;;
        esac
        # A removed unit can keep running as a loaded-runtime service; stop it
        # and verify it actually stopped.
        if test "$(systemctl --user is-active voisu.service 2>/dev/null || true)" = active; then
            systemctl --user stop voisu.service >/dev/null 2>&1 || true
        fi
        if test "$(systemctl --user is-active voisu.service 2>/dev/null || true)" = active; then
            printf 'cleanup: voisu.service is still active after removal\n' >&2
            if test "$rc" -eq 0; then rc=1; fi
        fi
    fi
    if ! restore_user_service; then
        printf 'user-service state could not be fully restored\n' >&2
        if test "$rc" -eq 0; then
            rc=1
        fi
    fi
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
# Capture rpm output once and grep the here-string. Piping `rpm | grep -q`
# under `set -o pipefail` can abort with 141 when grep exits on an early match
# and rpm dies of SIGPIPE mid-write; here-strings avoid the pipeline entirely.
rpm_files=$(rpm -qpl "$rpm_path")
grep -qx '/usr/bin/voisu' <<<"$rpm_files"
grep -qx '/usr/bin/voisu-daemon' <<<"$rpm_files"
grep -qx '/usr/lib/systemd/user/voisu.service' <<<"$rpm_files"
rpm_requires=$(rpm -qp --requires "$rpm_path")
grep -qx 'wl-clipboard' <<<"$rpm_requires"
grep -qx 'pipewire-utils' <<<"$rpm_requires"
grep -qx 'wireplumber' <<<"$rpm_requires"
grep -qx 'curl' <<<"$rpm_requires"
grep -qx '/usr/bin/secret-tool' <<<"$rpm_requires"
rpm_recommends=$(rpm -qp --recommends "$rpm_path")
grep -q '^libei' <<<"$rpm_recommends"

test -x /usr/bin/voisu
test -x /usr/bin/voisu-daemon
test -r /usr/lib/systemd/user/voisu.service
grep -qx 'ExecStart=/usr/bin/voisu-daemon --systemd' /usr/lib/systemd/user/voisu.service
# Fresh-home safety: the unit must provision its own config/state dirs. An
# explicit ReadWritePaths=%h/.config/voisu under ProtectSystem=strict failed
# systemd's namespace setup (status=226/NAMESPACE) on any home that never created
# those dirs. ConfigurationDirectory/StateDirectory make systemd create them
# BEFORE sandbox setup; ReadWritePaths must reference no %h home path.
grep -qx 'ConfigurationDirectory=voisu' /usr/lib/systemd/user/voisu.service
grep -qx 'StateDirectory=voisu' /usr/lib/systemd/user/voisu.service
if grep -Eq '^ReadWritePaths=.*%h' /usr/lib/systemd/user/voisu.service; then
    printf 'FAIL: voisu.service ReadWritePaths still references a %%h home path\n' >&2; exit 1
fi
/usr/bin/voisu --help >/dev/null
systemctl --user daemon-reload

# Snapshot every user-service state that `voisu service install`/`start` can
# mutate so the cleanup trap can restore it on both success and failure.
snapshot_dir=$(mktemp -d "${TMPDIR:-/tmp}/voisu-smoke-snapshot.XXXXXX")
pre_enabled=$(systemctl --user is-enabled voisu.service 2>/dev/null || true)
pre_active=$(systemctl --user is-active voisu.service 2>/dev/null || true)
if test "$overlay_installed_before" -eq 1; then
    pre_overlay_enabled=$(systemctl --user is-enabled voisu-overlay.service 2>/dev/null || true)
    pre_overlay_active=$(systemctl --user is-active voisu-overlay.service 2>/dev/null || true)
fi
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
# The unit started cleanly on this live session: systemd provisioned the config
# and state dirs (ConfigurationDirectory/StateDirectory) before the sandbox, so
# they exist even on a home that never created them by hand.
test -d "${XDG_CONFIG_HOME:-$HOME/.config}/voisu"
test -d "${XDG_STATE_HOME:-$HOME/.local/state}/voisu"
/usr/bin/voisu doctor
/usr/bin/voisu start
sleep "${VOISU_RECORDING_SECONDS:-3}"
/usr/bin/voisu stop
test -n "$(wl-paste --no-newline)"
/usr/bin/voisu service status
