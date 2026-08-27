#!/usr/bin/env bash
set -euo pipefail

readonly packaging_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
readonly gate_script=$packaging_dir/hyprland-release-gate.sh
readonly problems_doc=$(cd "$packaging_dir/.." && pwd)/docs/hyprland_problems.md
readonly release_doc=$(cd "$packaging_dir/.." && pwd)/docs/hyprland-release-gate.md
readonly test_root=$(mktemp -d "${TMPDIR:-/tmp}/voisu-hyprland-gate-test.XXXXXX")
readonly stub_dir=$test_root/bin
readonly payload_root=$test_root/payload
mkdir -p "$stub_dir"
mkdir -p "$payload_root/usr/bin" "$payload_root/usr/lib/systemd/user"
printf '%s\n' 'voisu cli fixture' >"$payload_root/usr/bin/voisu"
printf '%s\n' 'voisu daemon fixture' >"$payload_root/usr/bin/voisu-daemon"
printf '%s\n' 'voisu overlay fixture' >"$payload_root/usr/bin/voisu-overlay"
ln -s voisu "$payload_root/usr/bin/voisu-cli"
printf '%s\n' '[Unit]' >"$payload_root/usr/lib/systemd/user/voisu.service"
printf '%s\n' '[Unit]' >"$payload_root/usr/lib/systemd/user/voisu-overlay.service"
helper_pid=
good_helper_pid=
cleanup() {
    local pid
    for pid in ${helper_pid:-} ${good_helper_pid:-}; do
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    rm -rf "$test_root"
}
trap cleanup EXIT

fail() {
    printf 'FAIL: %s\n' "$1" >&2
    if [[ -n ${LAST_OUTPUT:-} ]]; then
        printf '%s\n' "$LAST_OUTPUT" >&2
    fi
    exit 1
}

make_stub() {
    local name=$1
    local body=$2
    printf '%s\n' "$body" >"$stub_dir/$name"
    chmod 0755 "$stub_dir/$name"
}

env -i PATH="/usr/bin:/bin" HOME="$test_root" \
    XDG_SESSION_TYPE=wayland \
    WAYLAND_DISPLAY=wayland-1 \
    HYPRLAND_INSTANCE_SIGNATURE=test-signature \
    sleep 3600 &
good_helper_pid=$!
[[ -r /proc/$good_helper_pid/environ ]] || fail 'session helper process environ must be readable'

make_stub pacman '#!/bin/bash
package=${@: -1}
pkgname=${TEST_PACKAGE:-voisu}
version=0.38.0-${TEST_RELEASE:-1}
if [[ ${1:-} == -Qp && ${2:-} == --info ]]; then
    [[ -s $package ]] || exit 1
    grep -q "^VOISU_TEST_ARTIFACT$" "$package" || exit 1
    printf "Name            : %s\\n" "$pkgname"
    printf "Version         : %s\\n" "$version"
    printf "%s\\n" "Architecture    : x86_64"
elif [[ ${1:-} == -Qi ]]; then
    [[ $package == "$pkgname" ]] || exit 1
    printf "Name            : %s\\n" "$pkgname"
    printf "Version         : %s\\n" "$version"
    printf "%s\\n" "Architecture    : x86_64"
elif [[ ${1:-} == -Ql ]]; then
    [[ $package == "$pkgname" ]] || exit 1
    root=${TEST_PAYLOAD_ROOT:?}
    for path in /usr/ /usr/bin/ /usr/bin/voisu /usr/bin/voisu-cli \
        /usr/bin/voisu-daemon /usr/bin/voisu-overlay /usr/lib/ /usr/lib/systemd/ \
        /usr/lib/systemd/user/ /usr/lib/systemd/user/voisu.service \
        /usr/lib/systemd/user/voisu-overlay.service; do
        printf "%s %s%s\\n" "$pkgname" "$root" "$path"
    done
else
    exit 1
fi'

make_stub bsdtar '#!/bin/bash
artifact_ok() {
    local artifact=$1
    local root
    local digest
    [[ -n $artifact && -s $artifact ]] || return 1
    grep -q "^VOISU_TEST_ARTIFACT$" "$artifact" || return 1
    root=${TEST_PAYLOAD_ROOT:?}
    digest=$(sha256sum "$root/usr/bin/voisu")
    digest=${digest%% *}
    grep -q "^${digest}[[:space:]]" "$artifact"
}
if [[ ${1:-} == -xOf && ${3:-} == .PKGINFO ]]; then
    artifact_ok "$2" || exit 1
    printf "pkgname = %s\\n" "${TEST_PACKAGE:-voisu}"
    case ${TEST_METADATA_MODE:-combined} in
        combined)
            printf "pkgver = 0.38.0-%s\\n" "${TEST_RELEASE:-1}"
            ;;
        split)
            printf "%s\\n" "pkgver = 0.38.0"
            printf "pkgrel = %s\\n" "${TEST_RELEASE:-1}"
            ;;
        missing-release)
            printf "%s\\n" "pkgver = 0.38.0"
            ;;
        malformed-release)
            printf "%s\\n" "pkgver = 0.38.0-"
            ;;
        *)
            exit 1
            ;;
    esac
    printf "%s\\n" "arch = x86_64"
elif [[ ${1:-} == -xOf && ${3:-} == .MTREE ]]; then
    artifact_ok "$2" || exit 1
    root=${TEST_PAYLOAD_ROOT:?}
    mtree_file() {
        local path=$1
        local digest
        local mtree_path=$path
        digest=$(sha256sum "$root$path")
        digest=${digest%% *}
        if [[ ${TEST_PAYLOAD_MODE:-good} == mismatch && $path == /usr/bin/voisu-daemon ]]; then
            digest=0000000000000000000000000000000000000000000000000000000000000000
        fi
        if [[ ${TEST_PAYLOAD_MODE:-good} == suffix-collision && $path == /usr/bin/voisu ]]; then
            mtree_path=/bin/voisu
        fi
        printf "./%s sha256digest=%s\\n" "${mtree_path#/}" "$digest"
    }
    {
        printf "%s\\n" "#mtree"
        printf "%s\\n" "/set type=file uid=0 gid=0 mode=644"
        printf "%s\\n" "./usr type=dir"
        printf "%s\\n" "./usr/bin type=dir"
        mtree_file /usr/bin/voisu
        printf "%s\\n" "./usr/bin/voisu-cli type=link link=voisu"
        mtree_file /usr/bin/voisu-daemon
        mtree_file /usr/bin/voisu-overlay
        printf "%s\\n" "./usr/lib type=dir"
        printf "%s\\n" "./usr/lib/systemd type=dir"
        printf "%s\\n" "./usr/lib/systemd/user type=dir"
        mtree_file /usr/lib/systemd/user/voisu.service
        mtree_file /usr/lib/systemd/user/voisu-overlay.service
    } | gzip -c
else
    exit 1
fi'

make_stub voisu '#!/bin/bash
if [[ ${1:-} == --version ]]; then
    printf "%s\\n" "voisu 0.38.0"
elif [[ ${1:-} == doctor ]]; then
    printf "%s\\n" "doctor: all checks passed"
else
    exit 1
fi'

make_stub voisu-daemon '#!/bin/bash
[[ ${1:-} == --version ]] && printf "%s\\n" "voisu-daemon 0.38.0"'

make_stub systemd-analyze '#!/bin/bash
exit 0'

make_stub systemctl '#!/bin/bash
if [[ ${1:-} == --user && ${2:-} == show ]]; then
    unit=${3:-}
    root=${TEST_PAYLOAD_ROOT:?}
    if [[ ${SYSTEMD_MODE:-good} == fragment-shadow ]]; then
        fragment=$root/home/test/.config/systemd/user/$unit
    else
        fragment=$root/usr/lib/systemd/user/$unit
    fi
    case "$unit" in
        voisu.service) binary=$root/usr/bin/voisu-daemon ;;
        voisu-overlay.service) binary=$root/usr/bin/voisu-overlay ;;
        *) exit 1 ;;
    esac
    if [[ ${SYSTEMD_MODE:-good} == exec-shadow ]]; then
        binary=$root/home/test/bin/${binary##*/}
    fi
    if [[ ${SYSTEMD_MODE:-good} == daemon-inactive && $unit == voisu.service \
        || ${SYSTEMD_MODE:-good} == overlay-inactive && $unit == voisu-overlay.service ]]; then
        printf "%s\\n" "ActiveState=inactive" "SubState=dead" "MainPID=0"
    elif [[ ${SYSTEMD_MODE:-good} == daemon-no-wayland && $unit == voisu.service \
        || ${SYSTEMD_MODE:-good} == daemon-no-session && $unit == voisu.service ]]; then
        printf "%s\\n" "ActiveState=active" "SubState=running" "MainPID=1"
    else
        printf "%s\\n" "ActiveState=active" "SubState=running" "MainPID=${TEST_DAEMON_PID:?}"
    fi
    if [[ $unit == voisu.service && ${SYSTEMD_MODE:-good} != daemon-no-session ]]; then
        if [[ ${SYSTEMD_MODE:-good} == daemon-no-wayland ]]; then
            printf "%s\\n" "Environment=XDG_SESSION_TYPE=wayland HYPRLAND_INSTANCE_SIGNATURE=test-signature"
        else
            printf "%s\\n" "Environment=WAYLAND_DISPLAY=wayland-1 XDG_SESSION_TYPE=wayland HYPRLAND_INSTANCE_SIGNATURE=test-signature"
        fi
    fi
    printf "FragmentPath=%s\\n" "$fragment"
    printf "ExecStart={ path=%s ; argv[]=%s --systemd ; ignore_errors=no ; }\\n" "$binary" "$binary"
    exit 0
fi
exit 1'

make_stub hyprctl '#!/bin/bash
if [[ ${1:-} == version ]]; then
    printf "%s\\n" "Hyprland 0.50.0"
elif [[ ${1:-} == binds ]]; then
    printf "%s\\n" "[]"
else
    exit 1
fi'

make_stub journalctl '#!/bin/bash
if [[ ${JOURNAL_MODE:-entries} == empty ]]; then
    printf "%s\\n" "-- No entries --"
elif [[ ${JOURNAL_MODE:-entries} == blank ]]; then
    exit 0
elif [[ ${JOURNAL_MODE:-entries} == warning ]]; then
    printf "%s\\n" "warning: journal access was degraded" >&2
elif [[ ${JOURNAL_MODE:-entries} == warning-with-entries ]]; then
    printf "%s\\n" "warning: journal access was degraded" >&2
    printf "%s\\n" "Aug 27 12:00:00 host voisu-daemon[1234]: journal entry"
elif [[ ${JOURNAL_MODE:-entries} == non-entry ]]; then
    printf "%s\\n" "journalctl: query completed"
else
    printf "%s\\n" "Aug 27 12:00:00 host voisu-daemon[1234]: journal entry"
fi'

test_release=1
test_package=voisu
metadata_mode=combined
payload_mode=good
systemd_mode=good
journal_mode=entries
test_daemon_pid=$good_helper_pid

run_gate() {
    local evidence_dir=$1
    set +e
    LAST_OUTPUT=$(env \
        PATH="$stub_dir:/usr/bin:/bin" \
        XDG_SESSION_TYPE=wayland \
        WAYLAND_DISPLAY=wayland-1 \
        HYPRLAND_INSTANCE_SIGNATURE=test-signature \
        TEST_RELEASE="$test_release" \
        TEST_PACKAGE="$test_package" \
        TEST_METADATA_MODE="$metadata_mode" \
        TEST_PAYLOAD_MODE="$payload_mode" \
        TEST_PAYLOAD_ROOT="$payload_root" \
        SYSTEMD_MODE="$systemd_mode" \
        JOURNAL_MODE="$journal_mode" \
        TEST_DAEMON_PID="$test_daemon_pid" \
        "$gate_script" --check "$evidence_dir" 2>&1)
    LAST_STATUS=$?
    set -e
}

write_package_artifact() {
    local dest=$1
    {
        printf '%s\n' 'VOISU_TEST_ARTIFACT'
        printf 'pkgname=%s\n' "$test_package"
        printf 'pkgver=%s\n' '0.38.0'
        printf 'pkgrel=%s\n' "$test_release"
        sha256sum "$payload_root/usr/bin/voisu"
    } >"$dest"
    [[ -s $dest ]] || fail 'package-artifact must contain real bytes'
}

new_evidence_dir() {
    local evidence_dir
    evidence_dir=$(mktemp -d "$test_root/evidence.XXXXXX")
    write_package_artifact "$evidence_dir/package-artifact"
    printf '%s\n' "$evidence_dir"
}

initialize_evidence() {
    local evidence_dir=$1
    run_gate "$evidence_dir"
    [[ $LAST_STATUS == 4 ]] || fail "fresh evidence initialization must wait for manual markers"
    [[ -f $evidence_dir/tested-release ]] || fail "fresh evidence initialization must write tested-release"
    grep -Fqx 'pkgver=0.38.0' "$evidence_dir/tested-release" \
        || fail 'tested-release must record the derived Arch package version'
    grep -Fqx 'pkgrel=1' "$evidence_dir/tested-release" \
        || fail 'tested-release must record the derived Arch package release'
}

make_pass_markers() {
    local evidence_dir=$1
    local except=${2:-}
    local gate
    for gate in clean-account-install trigger-key-conflict cold-login \
        daemon-wayland controlled-recording overlay-feedback verified-paste \
        clipboard-fallback compositor-recovery stale-daemon-doctor upgrade-reinstall; do
        [[ $gate == "$except" ]] || : >"$evidence_dir/$gate.pass"
    done
}

assert_status() {
    local expected=$1
    local description=$2
    [[ $LAST_STATUS == "$expected" ]] || fail "$description (expected $expected, got $LAST_STATUS)"
}

assert_output_contains() {
    local needle=$1
    local description=$2
    [[ $LAST_OUTPUT == *"$needle"* ]] || fail "$description (missing: $needle)"
}

assert_result() {
    local name=$1
    local status=$2
    local description=$3
    if ! grep -Eq "^${name}[[:space:]]+${status}$" <<<"$LAST_OUTPUT"; then
        fail "$description (missing result ${name}=${status})"
    fi
}

test_overlay_feedback_marker() {
    local evidence_dir
    evidence_dir=$(new_evidence_dir)
    initialize_evidence "$evidence_dir"
    make_pass_markers "$evidence_dir" overlay-feedback
    run_gate "$evidence_dir"
    assert_status 4 'Overlay feedback marker is required'
    assert_result overlay-feedback PENDING 'missing Overlay feedback must block'
    : >"$evidence_dir/overlay-feedback.pass"
    run_gate "$evidence_dir"
    assert_status 0 'complete Overlay feedback evidence must pass'
}

test_whitespace_waiver() {
    local evidence_dir
    evidence_dir=$(new_evidence_dir)
    initialize_evidence "$evidence_dir"
    make_pass_markers "$evidence_dir" cold-login
    printf ' \n\t' >"$evidence_dir/cold-login.waived"
    run_gate "$evidence_dir"
    assert_status 4 'whitespace-only waiver must block'
    assert_result cold-login PENDING 'whitespace-only waiver must remain pending'
    if grep -Eq '^cold-login[[:space:]]+WAIVED$' <<<"$LAST_OUTPUT"; then
        fail 'whitespace-only waiver must not be accepted'
    fi
    printf '%s\n' 'cold login unavailable on this test host' >"$evidence_dir/cold-login.waived"
    run_gate "$evidence_dir"
    assert_status 0 'reasoned waiver must pass'
}

test_packaged_unit_provenance() {
    local evidence_dir
    evidence_dir=$(new_evidence_dir)
    initialize_evidence "$evidence_dir"
    make_pass_markers "$evidence_dir"
    systemd_mode=fragment-shadow
    run_gate "$evidence_dir"
    assert_status 4 'shadow FragmentPath must block'
    assert_result packaged-units FAIL 'shadow FragmentPath must fail packaged-unit probe'
    systemd_mode=exec-shadow
    run_gate "$evidence_dir"
    assert_status 4 'shadow ExecStart must block'
    assert_result packaged-units FAIL 'shadow ExecStart must fail packaged-unit probe'
    systemd_mode=good
    run_gate "$evidence_dir"
    assert_status 0 'pacman-owned FragmentPath and ExecStart must pass'
}

test_service_state_and_session() {
    local evidence_dir
    evidence_dir=$(new_evidence_dir)
    initialize_evidence "$evidence_dir"
    make_pass_markers "$evidence_dir"
    systemd_mode=daemon-inactive
    run_gate "$evidence_dir"
    assert_status 4 'inactive daemon must block'
    assert_result daemon-service FAIL 'inactive daemon must fail service probe'
    systemd_mode=overlay-inactive
    run_gate "$evidence_dir"
    assert_status 4 'inactive Overlay must block'
    assert_result overlay-service FAIL 'inactive Overlay must fail service probe'
    systemd_mode=daemon-no-wayland
    run_gate "$evidence_dir"
    assert_status 4 'daemon without WAYLAND_DISPLAY must block'
    assert_result daemon-service FAIL 'missing daemon WAYLAND_DISPLAY must fail service probe'
    systemd_mode=daemon-no-session
    run_gate "$evidence_dir"
    assert_status 4 'daemon without session evidence must block'
    assert_result daemon-service FAIL 'missing daemon session evidence must fail service probe'
    systemd_mode=good
    run_gate "$evidence_dir"
    assert_status 0 'active services with matching session evidence must pass'
}

test_daemon_process_environment() {
    local evidence_dir
    evidence_dir=$(new_evidence_dir)
    initialize_evidence "$evidence_dir"
    make_pass_markers "$evidence_dir"
    env -i PATH="/usr/bin:/bin" HOME="$test_root" sleep 3600 &
    helper_pid=$!
    [[ -r /proc/$helper_pid/environ ]] || fail 'helper process environ must be readable'
    test_daemon_pid=$helper_pid
    systemd_mode=daemon-stale-proc
    run_gate "$evidence_dir"
    assert_status 4 'unit Environment= without matching process environ must block'
    assert_result daemon-service FAIL \
        'stale daemon process environment must fail service probe'
    systemd_mode=good
    test_daemon_pid=$good_helper_pid
    kill "$helper_pid" 2>/dev/null || true
    wait "$helper_pid" 2>/dev/null || true
    helper_pid=
}

test_release_binding() {
    local evidence_dir
    evidence_dir=$(new_evidence_dir)
    initialize_evidence "$evidence_dir"
    make_pass_markers "$evidence_dir"
    run_gate "$evidence_dir"
    assert_status 0 'initial package release evidence must pass'
    test_release=2
    run_gate "$evidence_dir"
    assert_status 4 'changed package release must invalidate old evidence'
    assert_output_contains 'different package release' 'changed package release must be reported'
    test_release=1
}

test_split_pkgver_format() {
    local evidence_dir
    metadata_mode=split
    evidence_dir=$(new_evidence_dir)
    initialize_evidence "$evidence_dir"
    make_pass_markers "$evidence_dir"
    run_gate "$evidence_dir"
    assert_status 0 'split pkgver/pkgrel metadata must pass'
    metadata_mode=combined
}

test_malformed_package_metadata() {
    local evidence_dir
    for metadata_mode in missing-release malformed-release; do
        evidence_dir=$(new_evidence_dir)
        run_gate "$evidence_dir"
        assert_status 4 'malformed combined package metadata must block'
        assert_output_contains 'version/release metadata is malformed' \
            'malformed package metadata must be reported'
    done
    metadata_mode=combined
}

test_payload_content_binding() {
    local evidence_dir
    evidence_dir=$(new_evidence_dir)
    initialize_evidence "$evidence_dir"
    make_pass_markers "$evidence_dir"
    payload_mode=mismatch
    run_gate "$evidence_dir"
    assert_status 4 'same-identity package with a different payload must block'
    assert_result payload-content FAIL \
        'different installed payload content must fail the artifact check'
    assert_output_contains 'installed payload content differs' \
        'different installed payload content must be reported'
    payload_mode=good
    run_gate "$evidence_dir"
    assert_status 0 'matching installed payload content must pass'
    assert_result payload-content PASS \
        'matching installed payload content must pass the artifact check'
}

test_mtree_path_suffix_collision() {
    local evidence_dir
    evidence_dir=$(new_evidence_dir)
    initialize_evidence "$evidence_dir"
    make_pass_markers "$evidence_dir"
    payload_mode=suffix-collision
    run_gate "$evidence_dir"
    assert_status 4 'suffix-colliding .MTREE path must block'
    assert_result payload-content FAIL \
        'artifact /bin/voisu must not match installed /usr/bin/voisu'
    assert_output_contains 'missing the artifact path' \
        'suffix-colliding .MTREE path must be reported as missing'
    payload_mode=good
}

test_symlink_payload() {
    local evidence_dir
    evidence_dir=$(new_evidence_dir)
    initialize_evidence "$evidence_dir"
    make_pass_markers "$evidence_dir"
    run_gate "$evidence_dir"
    assert_status 0 'matching installed symlink must pass'
    assert_result payload-content PASS \
        'matching installed symlink must pass the artifact check'
    ln -sf voisu-daemon "$payload_root/usr/bin/voisu-cli"
    run_gate "$evidence_dir"
    assert_status 4 'wrong installed symlink target must block'
    assert_result payload-content FAIL \
        'wrong installed symlink target must fail the artifact check'
    assert_output_contains 'installed payload link differs' \
        'wrong installed symlink target must be reported'
    ln -sfn voisu "$payload_root/usr/bin/voisu-cli"
}

test_empty_package_artifact() {
    local evidence_dir
    evidence_dir=$(mktemp -d "$test_root/evidence.XXXXXX")
    : >"$evidence_dir/package-artifact"
    run_gate "$evidence_dir"
    assert_status 4 'empty package artifact must block'
}

test_artifact_byte_replacement() {
    local evidence_dir
    evidence_dir=$(new_evidence_dir)
    initialize_evidence "$evidence_dir"
    make_pass_markers "$evidence_dir"
    run_gate "$evidence_dir"
    assert_status 0 'initial package artifact bytes must pass'
    {
        printf '%s\n' 'VOISU_TEST_ARTIFACT'
        printf 'pkgname=%s\n' "$test_package"
        printf 'pkgver=%s\n' '0.38.0'
        printf 'pkgrel=%s\n' "$test_release"
        sha256sum "$payload_root/usr/bin/voisu"
        printf '%s\n' 'tampered-same-identity'
    } >"$evidence_dir/package-artifact"
    run_gate "$evidence_dir"
    assert_status 4 'same-identity artifact byte replacement must block'
    assert_output_contains 'different package release' \
        'replaced artifact bytes must fail the artifact checksum binding'
    if grep -Eq 'Hyprland release gate PASS' <<<"$LAST_OUTPUT"; then
        fail 'replaced artifact bytes must not pass'
    fi
}

test_nonempty_journals() {
    local evidence_dir
    evidence_dir=$(new_evidence_dir)
    initialize_evidence "$evidence_dir"
    make_pass_markers "$evidence_dir"
    journal_mode=empty
    run_gate "$evidence_dir"
    assert_status 4 'empty journals must block'
    assert_result daemon-journal FAIL 'empty daemon journal must fail'
    assert_result overlay-journal FAIL 'empty Overlay journal must fail'
    journal_mode=blank
    run_gate "$evidence_dir"
    assert_status 4 'blank journals must block'
    assert_result daemon-journal FAIL 'blank daemon journal must fail'
    assert_result overlay-journal FAIL 'blank Overlay journal must fail'
    journal_mode=warning
    run_gate "$evidence_dir"
    assert_status 4 'warning-only journals must block'
    assert_result daemon-journal FAIL 'warning-only daemon journal must fail'
    assert_result overlay-journal FAIL 'warning-only Overlay journal must fail'
    journal_mode=non-entry
    run_gate "$evidence_dir"
    assert_status 4 'non-entry journal output must block'
    assert_result daemon-journal FAIL 'non-entry daemon output must fail'
    assert_result overlay-journal FAIL 'non-entry Overlay output must fail'
    journal_mode=warning-with-entries
    run_gate "$evidence_dir"
    assert_status 4 'journals with stderr diagnostics must block'
    assert_result daemon-journal FAIL 'stderr diagnostics must fail daemon journal'
    assert_result overlay-journal FAIL 'stderr diagnostics must fail Overlay journal'
    grep -Eq $'^daemon-journal\tFAIL\tjournal diagnostics$' "$evidence_dir/results.tsv" \
        || fail 'non-whitespace journal stderr must fail as journal diagnostics'
    journal_mode=entries
    run_gate "$evidence_dir"
    assert_status 0 'nonempty journals must pass'
}

test_fresh_directory_required() {
    local evidence_dir
    evidence_dir=$(new_evidence_dir)
    : >"$evidence_dir/cold-login.pass"
    run_gate "$evidence_dir"
    assert_status 4 'manual markers before release initialization must block'
    assert_output_contains 'initialize a fresh evidence directory' 'unbound marker directory must be rejected'
}

test_paste_documentation() {
    local stale_phrase
    for stale_phrase in \
        'writes the final Transcript to the clipboard and does not insert it into the focused application.' \
        'writes the final Transcript to the clipboard and does not insert it into the focused application' \
        'Do not silently fall back to simulated typing or direct paste.' \
        'Voisu will not paste directly.'; do
        if grep -Fq "$stale_phrase" "$problems_doc"; then
            fail "Hyprland documentation contains stale clipboard-only wording: $stale_phrase"
        fi
    done
    grep -Fq 'If a configured Paste Action is verified' "$problems_doc" \
        || fail 'Hyprland documentation must make Paste Action insertion conditional'
    grep -Fq 'If the Paste Action is unavailable or fails' "$problems_doc" \
        || fail 'Hyprland documentation must document clipboard fallback for Paste Action failure'
    grep -Fq 'If setup verified' "$problems_doc" \
        && grep -Fq 'a Paste Action, confirm it inserts that same Transcript into the focused' "$problems_doc" \
        || fail 'Hyprland checklist must document verified Paste Action insertion'
    grep -Fq 'If no verified Paste Action is available or paste fails,' "$problems_doc" \
        && grep -Fq 'confirm the Transcript remains on the clipboard.' "$problems_doc" \
        || fail 'Hyprland checklist must document clipboard fallback'
    grep -Fq '| `overlay-feedback.pass` | Overlay shows Recording, Processing, and terminal feedback |' "$release_doc" \
        || fail 'release gate documentation must list Overlay feedback evidence'
    if grep -Eiq '\b(rpm|nevra|fedora)\b' "$release_doc"; then
        fail 'release gate documentation must describe the Arch package path, not RPM'
    fi
    grep -Fq 'pkgname' "$release_doc" \
        && grep -Fq 'pkgver' "$release_doc" \
        && grep -Fq 'pkgrel' "$release_doc" \
        || fail 'release gate documentation must record Arch package identity'
}

test_overlay_feedback_marker
test_whitespace_waiver
test_packaged_unit_provenance
test_service_state_and_session
test_daemon_process_environment
test_release_binding
test_split_pkgver_format
test_malformed_package_metadata
test_payload_content_binding
test_mtree_path_suffix_collision
test_symlink_payload
test_empty_package_artifact
test_artifact_byte_replacement
test_nonempty_journals
test_fresh_directory_required
test_paste_documentation
printf '%s\n' 'Hyprland release-gate tests: PASS'
