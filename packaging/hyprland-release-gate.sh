#!/usr/bin/env bash
set -euo pipefail

# Evidence runner for the packaged Hyprland cold-login/recovery gate.
#
# This script is deliberately read-only with respect to packages, services,
# compositor configuration, and the user's session. The documented manual
# gates (clean-account install, reboot, Recording, recovery, and upgrade) must
# be performed by an operator and represented by explicit .pass or .waived
# markers in the evidence directory. A missing marker is never treated as a
# pass, so this tool cannot accidentally certify an untested release.

readonly SCRIPT_NAME=${0##*/}
readonly -a MANUAL_GATES=(
    clean-account-install
    trigger-key-conflict
    cold-login
    daemon-wayland
    controlled-recording
    verified-paste
    clipboard-fallback
    compositor-recovery
    stale-daemon-doctor
    upgrade-reinstall
)

usage() {
    cat <<EOF
usage:
  $SCRIPT_NAME --plan
  $SCRIPT_NAME --check EVIDENCE_DIR

--plan                 print the documented manual and automated gates
--check EVIDENCE_DIR   collect read-only host evidence and evaluate markers

Manual evidence markers are named <gate>.pass or <gate>.waived in
EVIDENCE_DIR. A .waived marker must contain a non-empty reason. The check
passes only when every automated probe succeeds and every manual gate is PASS
or explicitly WAIVED.
EOF
}

print_plan() {
    cat <<'EOF'
Hyprland packaged release gate

Automated probes collected by --check:
  - Fedora/Hyprland session variables and current display
  - voisu and voisu-daemon versions
  - installed Voisu and Overlay package metadata
  - packaged user-unit verification and systemd user state
  - Hyprland version and live bindings
  - voisu doctor --verbose
  - daemon and Overlay user journals

Manual gates, each requiring <name>.pass or <name>.waived evidence:
  clean-account-install  install the exact package and required Overlay package
  trigger-key-conflict   verify Left Alt and Right Alt conflict handling
  cold-login              reboot, log into Hyprland, and observe both services
  daemon-wayland          prove the daemon owns the current WAYLAND_DISPLAY
  controlled-recording    produce exactly one final clipboard Transcript
  verified-paste          verify the configured Paste Action inserts it
  clipboard-fallback      prove paste failure/unavailability preserves clipboard
  compositor-recovery     restart Hyprland and its portal, then verify recovery
  stale-daemon-doctor     create stale daemon state and verify doctor fails
  upgrade-reinstall       upgrade/reinstall without losing credentials or bindings

The runner never installs packages, changes Hyprland configuration, restarts a
service, or reboots the machine. Those actions belong to the documented gate
operator and must be recorded in the marker evidence.
EOF
}

if [[ ${1:-} == "--plan" ]]; then
    print_plan
    exit 0
fi
if [[ ${1:-} != "--check" || -z ${2:-} || ${3:-} != "" ]]; then
    usage >&2
    exit 2
fi

readonly evidence_dir=$2
mkdir -p "$evidence_dir"
readonly results_file=$evidence_dir/results.tsv
: >"$results_file"

record_result() {
    local name=$1
    local status=$2
    local detail=${3:-}
    printf '%s\t%s\t%s\n' "$name" "$status" "$detail" >>"$results_file"
    printf '%-24s %s\n' "$name" "$status"
}

run_probe() {
    local name=$1
    shift
    local log="$evidence_dir/$name.log"
    {
        printf '$'
        printf ' %q' "$@"
        printf '\n'
    } >"$log"

    set +e
    "$@" >>"$log" 2>&1
    local status=$?
    set -e
    printf 'exit=%s\n' "$status" >>"$log"
    cat "$log"
    if ((status == 0)); then
        record_result "$name" PASS "exit=0"
    else
        record_result "$name" FAIL "exit=$status"
    fi
}

run_session_probe() {
    local log="$evidence_dir/session.log"
    {
        printf 'XDG_SESSION_TYPE=%q\n' "${XDG_SESSION_TYPE-}"
        printf 'WAYLAND_DISPLAY=%q\n' "${WAYLAND_DISPLAY-}"
        printf 'HYPRLAND_INSTANCE_SIGNATURE=%q\n' "${HYPRLAND_INSTANCE_SIGNATURE-}"
    } >"$log"
    if [[ ${XDG_SESSION_TYPE-} == wayland \
        && -n ${WAYLAND_DISPLAY-} \
        && -n ${HYPRLAND_INSTANCE_SIGNATURE-} ]]; then
        record_result session PASS "Wayland display is present"
    else
        record_result session FAIL "not a live Hyprland Wayland session"
    fi
    cat "$log"
}

command_path() {
    local command=$1
    command -v "$command" 2>/dev/null || printf '%s' /usr/bin/false
}

run_session_probe
run_probe voisu-version "$(command_path voisu)" --version
run_probe voisu-daemon-version "$(command_path voisu-daemon)" --version
run_probe voisu-package "$(command_path rpm)" -q voisu
run_probe overlay-package "$(command_path rpm)" -q voisu-overlay
run_probe user-units "$(command_path systemd-analyze)" --user verify voisu.service voisu-overlay.service
run_probe daemon-service "$(command_path systemctl)" --user show voisu.service \
    -p ActiveState -p SubState -p MainPID -p Environment
run_probe overlay-service "$(command_path systemctl)" --user show voisu-overlay.service \
    -p ActiveState -p SubState -p MainPID
run_probe hyprland-version "$(command_path hyprctl)" version
run_probe hyprland-bindings "$(command_path hyprctl)" binds -j
run_probe doctor "$(command_path voisu)" doctor --verbose
run_probe daemon-journal "$(command_path journalctl)" --user -u voisu.service -n 200 --no-pager
run_probe overlay-journal "$(command_path journalctl)" --user -u voisu-overlay.service -n 200 --no-pager

pending=0
for gate in "${MANUAL_GATES[@]}"; do
    pass_marker="$evidence_dir/$gate.pass"
    waive_marker="$evidence_dir/$gate.waived"
    if [[ -f $pass_marker ]]; then
        record_result "$gate" PASS "$(basename "$pass_marker")"
    elif [[ -f $waive_marker && -s $waive_marker ]]; then
        record_result "$gate" WAIVED "$(basename "$waive_marker")"
    else
        record_result "$gate" PENDING "create .pass or non-empty .waived marker"
        pending=1
    fi
done

if awk -F '\t' '$2 == "FAIL" { found = 1 } END { exit !found }' "$results_file"; then
    printf '%s\n' 'Hyprland release gate BLOCKED: an automated probe failed.' >&2
    exit 4
fi
if ((pending)); then
    printf '%s\n' 'Hyprland release gate BLOCKED: manual evidence is incomplete.' >&2
    exit 4
fi

printf '%s\n' "Hyprland release gate PASS: evidence recorded in $evidence_dir"
