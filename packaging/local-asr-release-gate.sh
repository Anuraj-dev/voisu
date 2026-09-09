#!/usr/bin/env bash
set -euo pipefail

# Fail-closed Local ASR packaging/rollback gate.
#
# Repository checks prove contracts. Real-host install, microphone, Trigger Key
# uniqueness, suspend, and process-tree network traces are orchestrator-owned.
# This script never installs packages, never talks to a microphone, and never
# treats a missing marker as a pass. A Hyprland pilot pass is not Fedora proof.

readonly SCRIPT_NAME=${0##*/}
readonly -a REPO_GATES=(
    clean-install
    offline-restart
    upgrade-private-settings
    bad-model-repair
    retained-model-restore
    supported-binary-rollback
    worker-death
    uninstall-ownership
)
readonly -a HOST_GATES=(
    fedora-kde-wayland-product
    omarchy-arch-hyprland-pilot
    real-microphone
    trigger-key-uniqueness
    suspend
    process-tree-offline
)

usage() {
    cat <<EOF
usage:
  $SCRIPT_NAME --plan
  $SCRIPT_NAME --check EVIDENCE_DIR

--plan                 print the supported-profile packaging/rollback gates
--check EVIDENCE_DIR   evaluate markers fail-closed; never claim a host pass

Markers are <gate>.pass or <gate>.waived in EVIDENCE_DIR. A .waived marker must
contain a non-whitespace reason. Host gates cannot be auto-passed. Fedora KDE
Wayland is the first supported product; Omarchy/Arch Hyprland is a pilot.
EOF
}

print_plan() {
    cat <<'EOF'
Local ASR supported-profile packaging gate

First supported product: Fedora KDE Wayland
Pilot, not Fedora proof: Omarchy/Arch Hyprland

Repository contracts (unit/integration tests plus markers):
  clean-install                 packaged binaries, no production weights
  offline-restart               Local selection survives a daemon restart
  upgrade-private-settings      config, dictionary, models, credentials kept
  bad-model-repair              Cloud switch is not a repair
  retained-model-restore        explicit Setup with a verified retained receipt
  supported-binary-rollback     mode-aware compatible binary only
  worker-death                  Local stays selected; readiness unavailable
  uninstall-ownership           user models/debug audio/leases are not packaged

Orchestrator-owned host gates (missing marker = FAIL, never PASS):
  fedora-kde-wayland-product    clean-account packaged install on Fedora KDE
  omarchy-arch-hyprland-pilot   pilot only; does not prove Fedora
  real-microphone               live microphone Recording
  trigger-key-uniqueness        exact Trigger Key uniqueness
  suspend                       suspend/resume
  process-tree-offline          process-tree network trace

Rollback:
  Cloud is a user opt-out for subsequent Recordings, not a repair.
  Repair/restore Local through explicit Setup with a retained verified receipt.
  Binary rollback only to a mode-aware compatible version.
  Pre-mode downgrades require an explicit migration and cannot keep this
  offline guarantee.

This runner never claims a real-host pass from CI.
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
if [[ ! -d $evidence_dir ]]; then
    printf 'FAIL: evidence directory does not exist: %s\n' "$evidence_dir" >&2
    exit 1
fi

readonly results_file=$evidence_dir/results.tsv
: >"$results_file"

failed=0

record_result() {
    local name=$1
    local status=$2
    local detail=${3:-}
    printf '%s\t%s\t%s\n' "$name" "$status" "$detail" >>"$results_file"
    printf '%-32s %s\n' "$name" "$status"
    if [[ $status == FAIL ]]; then
        failed=1
    fi
}

evaluate_marker() {
    local name=$1
    local pass=$evidence_dir/$name.pass
    local waived=$evidence_dir/$name.waived
    if [[ -e $pass && -e $waived ]]; then
        record_result "$name" FAIL "both .pass and .waived present"
        return
    fi
    if [[ -e $pass ]]; then
        if [[ ! -s $pass ]]; then
            record_result "$name" FAIL "empty .pass marker"
            return
        fi
        record_result "$name" PASS "marker"
        return
    fi
    if [[ -e $waived ]]; then
        if grep -q '[^[:space:]]' "$waived"; then
            record_result "$name" WAIVED "$(tr '\n' ' ' <"$waived")"
        else
            record_result "$name" FAIL "empty .waived marker"
        fi
        return
    fi
    record_result "$name" FAIL "missing marker"
}

for gate in "${REPO_GATES[@]}" "${HOST_GATES[@]}"; do
    evaluate_marker "$gate"
done

if ((failed == 1)); then
    printf 'Local ASR packaging gate: FAIL (missing or invalid evidence)\n' >&2
    exit 1
fi

printf 'Local ASR packaging gate: all required markers present\n'
exit 0
