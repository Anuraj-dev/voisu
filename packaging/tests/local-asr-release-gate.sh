#!/usr/bin/env bash
set -euo pipefail

readonly packaging_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
readonly gate_script=$packaging_dir/local-asr-release-gate.sh
readonly test_root=$(mktemp -d "${TMPDIR:-/tmp}/voisu-local-asr-gate-test.XXXXXX")
cleanup() { rm -rf "$test_root"; }
trap cleanup EXIT

fail() {
    printf 'FAIL: %s\n' "$1" >&2
    if [[ -n ${LAST_OUTPUT:-} ]]; then
        printf '%s\n' "$LAST_OUTPUT" >&2
    fi
    exit 1
}

LAST_OUTPUT=
LAST_STATUS=0
run_gate() {
    set +e
    LAST_OUTPUT=$("$gate_script" "$@" 2>&1)
    LAST_STATUS=$?
    set -e
}

run_gate --plan
[[ $LAST_STATUS -eq 0 ]] || fail '--plan must succeed'
printf '%s\n' "$LAST_OUTPUT" | grep -q 'Fedora KDE Wayland' || fail 'plan must name Fedora first'
printf '%s\n' "$LAST_OUTPUT" | grep -q 'pilot' || fail 'plan must name the Hyprland pilot'
printf '%s\n' "$LAST_OUTPUT" | grep -q 'not a repair' || fail 'plan must say Cloud is not a repair'
printf '%s\n' "$LAST_OUTPUT" | grep -q 'never claims a real-host pass' || fail 'plan must not claim host pass'

run_gate --check "$test_root/missing"
[[ $LAST_STATUS -ne 0 ]] || fail 'missing evidence dir must fail'

empty=$test_root/empty
mkdir -p "$empty"
run_gate --check "$empty"
[[ $LAST_STATUS -ne 0 ]] || fail 'empty evidence dir must fail'
printf '%s\n' "$LAST_OUTPUT" | grep -q 'clean-install' || fail 'empty check must name missing repo gates'
printf '%s\n' "$LAST_OUTPUT" | grep -q 'real-microphone' || fail 'empty check must name missing host gates'
printf '%s\n' "$LAST_OUTPUT" | grep -q 'FAIL' || fail 'empty check must FAIL'

repo=$test_root/repo
mkdir -p "$repo"
for gate in clean-install offline-restart upgrade-private-settings bad-model-repair \
    retained-model-restore supported-binary-rollback worker-death uninstall-ownership
do
    printf 'repository contract\n' >"$repo/$gate.pass"
done
run_gate --check "$repo"
[[ $LAST_STATUS -ne 0 ]] || fail 'repo markers without host evidence must fail'
printf '%s\n' "$LAST_OUTPUT" | grep -q 'fedora-kde-wayland-product' || fail 'host product gate must stay required'
printf '%s\n' "$LAST_OUTPUT" | grep -q 'trigger-key-uniqueness' || fail 'Trigger Key uniqueness stays a host gate'

pilot_only=$test_root/pilot-only
mkdir -p "$pilot_only"
cp "$repo"/*.pass "$pilot_only/"
printf 'hyprland pilot session\n' >"$pilot_only/omarchy-arch-hyprland-pilot.pass"
run_gate --check "$pilot_only"
[[ $LAST_STATUS -ne 0 ]] || fail 'Hyprland pilot must not certify Fedora'
printf '%s\n' "$LAST_OUTPUT" | grep -q 'fedora-kde-wayland-product' || fail 'Fedora product marker must still be required'

pilot=$test_root/pilot
mkdir -p "$pilot"
cp "$repo"/*.pass "$pilot/"
printf 'reason: not a host runner\n' >"$pilot/fedora-kde-wayland-product.waived"
printf 'reason: not a host runner\n' >"$pilot/omarchy-arch-hyprland-pilot.waived"
printf 'reason: orchestrator-owned\n' >"$pilot/real-microphone.waived"
printf 'reason: orchestrator-owned\n' >"$pilot/trigger-key-uniqueness.waived"
printf 'reason: orchestrator-owned\n' >"$pilot/suspend.waived"
printf 'reason: orchestrator-owned\n' >"$pilot/process-tree-offline.waived"
run_gate --check "$pilot"
[[ $LAST_STATUS -eq 0 ]] || fail "explicit host waivers with reasons must pass: $LAST_OUTPUT"
printf '%s\n' "$LAST_OUTPUT" | grep -q 'WAIVED' || fail 'waived host gates must be visible'

empty_waiver=$test_root/empty-waiver
mkdir -p "$empty_waiver"
cp "$pilot"/*.pass "$empty_waiver/"
cp "$pilot"/*.waived "$empty_waiver/"
: >"$empty_waiver/real-microphone.waived"
run_gate --check "$empty_waiver"
[[ $LAST_STATUS -ne 0 ]] || fail 'empty host waiver must fail'

printf 'PASS: local ASR packaging gate script\n'
