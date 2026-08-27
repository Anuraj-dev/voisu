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
    trigger-key-accepted
    trigger-key-rejected
    trigger-key-occupied
    trigger-key-both-occupied
    trigger-key-managed-rerun
    cold-login
    daemon-wayland
    controlled-recording
    overlay-feedback
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
EVIDENCE_DIR. A .waived marker must contain a non-whitespace reason. The first
check must use a fresh directory with no markers; it records the installed
package identity in tested-release. Later checks reject a changed identity.
The check passes only when every automated probe succeeds and every manual gate
is PASS or explicitly WAIVED.
EOF
}

print_plan() {
    cat <<'EOF'
Hyprland packaged release gate

Automated probes collected by --check:
  - Arch/Hyprland session variables and current display
  - voisu and voisu-daemon versions
  - installed Voisu package identity and exact package artifact
  - installed payload manifest and SHA-256 digests match the artifact
  - packaged user-unit and binary provenance plus systemd user state
  - Hyprland version and live bindings
  - voisu doctor --verbose
  - daemon and Overlay user journal entries

Manual gates, each requiring <name>.pass or <name>.waived evidence:
  clean-account-install       install the exact `voisu` or `voisu-bin` package
  trigger-key-accepted        Caps Lock accepted as Trigger Key; record hyprctl binds -j
  trigger-key-rejected        Caps Lock declined; Right Alt installed; never auto-install Left Alt
  trigger-key-occupied        exact unmanaged Caps Lock kept; Right Alt fallback
  trigger-key-both-occupied   Caps Lock and Right Alt owned; fail closed; no overwrite or Left Alt
  trigger-key-managed-rerun   existing managed Caps Lock or Right Alt kept on setup rerun
  cold-login                  reboot, log into Hyprland, and observe both services
  daemon-wayland              prove the daemon owns the current WAYLAND_DISPLAY
  controlled-recording        produce exactly one final clipboard Transcript
  overlay-feedback            observe Recording, Processing, and terminal Overlay feedback
  verified-paste              verify the configured Paste Action inserts it
  clipboard-fallback          prove paste failure/unavailability preserves clipboard
  compositor-recovery         restart Hyprland and its portal, then verify recovery
  stale-daemon-doctor         create stale daemon state and verify doctor fails
  upgrade-reinstall           upgrade/reinstall without losing credentials or bindings

The runner never installs packages, changes Hyprland configuration, restarts a
service, or reboots the machine. Those actions belong to the documented gate
operator and must be recorded in the marker evidence. Left Alt is not an
automatically installed Trigger Key.
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
readonly release_file=$evidence_dir/tested-release
readonly package_artifact=$evidence_dir/package-artifact

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

package_info_value() {
    local key=$1
    local info=$2
    awk -F: -v key="$key" '
        {
            field = $1
            sub(/[[:space:]]+$/, "", field)
            if (field == key) {
                value = substr($0, index($0, ":") + 1)
                sub(/^[[:space:]]+/, "", value)
                sub(/[[:space:]]+$/, "", value)
                if (found++) malformed = 1
                else result = value
            }
        }
        END {
            if (!found || malformed) exit 1
            print result
        }
    ' <<<"$info"
}

pkginfo_value() {
    local key=$1
    local info=$2
    awk -F= -v key="$key" '
        {
            field = $1
            sub(/[[:space:]]+$/, "", field)
            if (field == key) {
                value = substr($0, index($0, "=") + 1)
                sub(/^[[:space:]]+/, "", value)
                sub(/[[:space:]]+$/, "", value)
                if (found++) malformed = 1
                else result = value
            }
        }
        END {
            if (!found || malformed) exit 1
            print result
        }
    ' <<<"$info"
}

pkginfo_key_count() {
    local key=$1
    local info=$2
    awk -F= -v key="$key" '
        {
            field = $1
            sub(/[[:space:]]+$/, "", field)
            if (field == key) found++
        }
        END { print found + 0 }
    ' <<<"$info"
}

read_package_identity() {
    local pacman_path
    local bsdtar_path
    local artifact_info
    local pkginfo
    local installed_info
    local installed_package=
    local candidate
    local candidate_info
    local installed_name
    local installed_version
    local installed_arch
    local package_name
    local package_version_raw
    local package_version
    local package_release
    local package_release_raw
    local package_release_count
    local package_epoch
    local package_epoch_count
    local package_arch
    local artifact_name
    local artifact_version
    local artifact_arch
    local expected_installed_version
    local artifact_sha256

    pacman_path=$(command_path pacman)
    bsdtar_path=$(command_path bsdtar)
    if [[ ! -f $package_artifact ]]; then
        printf 'missing exact Arch package artifact: %s\n' "$package_artifact" >&2
        return 1
    fi
    if ! artifact_info=$("$pacman_path" -Qp --info "$package_artifact"); then
        printf 'cannot inspect Arch package artifact: %s\n' "$package_artifact" >&2
        return 1
    fi
    if ! pkginfo=$("$bsdtar_path" -xOf "$package_artifact" .PKGINFO); then
        printf 'cannot read .PKGINFO from Arch package artifact: %s\n' "$package_artifact" >&2
        return 1
    fi

    for candidate in voisu voisu-bin; do
        if candidate_info=$("$pacman_path" -Qi "$candidate" 2>/dev/null); then
            if [[ -n $installed_package ]]; then
                printf 'both voisu and voisu-bin are installed\n' >&2
                return 1
            fi
            installed_package=$candidate
            installed_info=$candidate_info
        fi
    done
    if [[ -z $installed_package ]]; then
        printf 'neither voisu nor voisu-bin is installed\n' >&2
        return 1
    fi

    if ! installed_name=$(package_info_value Name "$installed_info") \
        || ! installed_version=$(package_info_value Version "$installed_info") \
        || ! installed_arch=$(package_info_value Architecture "$installed_info") \
        || ! package_name=$(pkginfo_value pkgname "$pkginfo") \
        || ! package_version_raw=$(pkginfo_value pkgver "$pkginfo") \
        || ! package_arch=$(pkginfo_value arch "$pkginfo") \
        || ! artifact_name=$(package_info_value Name "$artifact_info") \
        || ! artifact_version=$(package_info_value Version "$artifact_info") \
        || ! artifact_arch=$(package_info_value Architecture "$artifact_info"); then
        printf 'Arch package identity is incomplete\n' >&2
        return 1
    fi

    package_release_count=$(pkginfo_key_count pkgrel "$pkginfo")
    if [[ $package_release_count == 1 ]]; then
        if ! package_release_raw=$(pkginfo_value pkgrel "$pkginfo"); then
            printf 'Arch package release metadata is malformed\n' >&2
            return 1
        fi
        package_version=$package_version_raw
        package_release=$package_release_raw
    elif [[ $package_release_count == 0 \
        && $package_version_raw =~ ^(.+)-([0-9]+([.][0-9]+)?)$ ]]; then
        package_version=${BASH_REMATCH[1]}
        package_release=${BASH_REMATCH[2]}
    else
        printf 'Arch package version/release metadata is malformed\n' >&2
        return 1
    fi

    package_epoch_count=$(pkginfo_key_count epoch "$pkginfo")
    if [[ $package_epoch_count == 0 ]]; then
        package_epoch=0
    elif [[ $package_epoch_count == 1 ]] \
        && package_epoch=$(pkginfo_value epoch "$pkginfo"); then
        :
    else
        printf 'Arch package epoch metadata is malformed\n' >&2
        return 1
    fi
    if [[ $package_name != voisu && $package_name != voisu-bin \
        || $package_arch != x86_64 \
        || -z $package_version \
        || $package_version =~ [[:space:]] \
        || ! $package_release =~ ^[0-9]+([.][0-9]+)?$ \
        || ! $package_epoch =~ ^[0-9]+$ ]]; then
        printf 'unsupported or malformed Voisu Arch package identity\n' >&2
        return 1
    fi
    if [[ $package_epoch == 0 ]]; then
        expected_installed_version=$package_version-$package_release
    else
        expected_installed_version=$package_epoch:$package_version-$package_release
    fi
    if [[ $artifact_name != "$package_name" \
        || $artifact_version != "$expected_installed_version" \
        || $artifact_arch != "$package_arch" \
        || $installed_name != "$package_name" \
        || $installed_package != "$package_name" \
        || $installed_version != "$expected_installed_version" \
        || $installed_arch != "$package_arch" ]]; then
        printf 'installed package does not match the exact Arch package artifact\n' >&2
        return 1
    fi
    local sha256sum_path
    sha256sum_path=$(command_path sha256sum)
    if ! artifact_sha256=$("$sha256sum_path" "$package_artifact" | awk '{ print $1 }') \
        || [[ ! $artifact_sha256 =~ ^[[:xdigit:]]{64}$ ]]; then
        printf 'cannot calculate the Arch package artifact checksum\n' >&2
        return 1
    fi

    printf 'pkgname=%s\n' "$package_name"
    printf 'pkgver=%s\n' "$package_version"
    printf 'pkgrel=%s\n' "$package_release"
    printf 'arch=%s\n' "$package_arch"
    printf 'sha256=%s\n' "$artifact_sha256"
}

mtree_path_to_logical_path() {
    local path=$1
    local relative

    [[ $path == ./* ]] || return 1
    relative=${path#./}
    [[ -n $relative && $relative != *'\'* && $relative != *[[:space:]]* ]] || return 1
    case "/$relative" in
        /../*|*/../*|*/..) return 1 ;;
    esac
    printf '/%s' "$relative"
}

parse_artifact_mtree() {
    local mtree_file=$1
    local entries_file=$2

    awk '
        BEGIN { default_type = "file" }
        /^#/ { next }
        /^[[:space:]]*$/ { next }
        /^\/set([[:space:]]|$)/ {
            for (i = 2; i <= NF; i++) {
                if ($i ~ /^type=/) default_type = substr($i, 6)
            }
            next
        }
        /^\/unset([[:space:]]|$)/ {
            for (i = 2; i <= NF; i++) {
                if ($i == "type") default_type = ""
            }
            next
        }
        {
            path = $1
            if (path !~ /^\.\// || path == "./" || path ~ /[[:space:]]/ || path ~ /\\/) {
                malformed = 1
                next
            }
            entry_type = default_type
            digest = ""
            link = ""
            for (i = 2; i <= NF; i++) {
                if ($i ~ /^type=/) entry_type = substr($i, 6)
                else if ($i ~ /^sha256digest=/) digest = substr($i, 14)
                else if ($i ~ /^link=/) link = substr($i, 6)
            }
            if (path == "./.BUILDINFO" || path == "./.INSTALL" \
                || path == "./.MTREE" || path == "./.PKGINFO") next
            if (entry_type == "file") {
                if (digest !~ /^[[:xdigit:]]{64}$/) malformed = 1
                else print "file\t" path "\t" tolower(digest)
            } else if (entry_type == "dir") {
                print "dir\t" path
            } else if (entry_type == "link" && link != "") {
                print "link\t" path "\t" link
            } else {
                malformed = 1
            }
            entries++
        }
        END { if (malformed || entries == 0) exit 1 }
    ' "$mtree_file" >"$entries_file"
}

common_installed_root_prefix() {
    local paths_file=$1
    local common

    common=$(awk '
        function parent(path) {
            if (path == "/") return "/"
            sub(/\/[^\/]*$/, "", path)
            if (path == "") return "/"
            return path
        }
        function is_dir_prefix(path, prefix) {
            if (prefix == "/") return path ~ /^\//
            return path == prefix || index(path, prefix "/") == 1
        }
        BEGIN { first = 1 }
        {
            if ($0 !~ /^\// || $0 ~ /(^|\/)\.\.(\/|$)/) {
                bad = 1
                next
            }
            p = parent($0)
            if (first) {
                common = p
                first = 0
                next
            }
            while (common != "" && !is_dir_prefix(p, common)) {
                if (common == "/") {
                    common = ""
                    break
                }
                common = parent(common)
            }
        }
        END {
            if (bad || first || common == "") exit 1
            print common
        }
    ' "$paths_file") || return 1
    if [[ $common == / ]]; then
        return 0
    fi
    if [[ $common != /* || $common == */ || $common == *[[:space:]]* ]]; then
        return 1
    fi
    case "$common" in
        ..|../*|*/..|*/../*) return 1 ;;
    esac
    if ! awk -v prefix="$common" '
        $0 != prefix && index($0, prefix "/") != 1 { exit 1 }
    ' "$paths_file"; then
        return 1
    fi
    printf '%s\n' "$common"
}

verify_installed_payload() {
    local package=$1
    local pacman_path
    local bsdtar_path
    local gzip_path
    local sha256sum_path
    local readlink_path
    local temp_dir
    local mtree_file
    local entries_file
    local artifact_manifest
    local sorted_artifact_manifest
    local artifact_paths
    local installed_paths
    local package_files
    local type
    local mtree_path
    local logical_path
    local digest
    local link
    local installed_path
    local installed_root
    local payload_prefix
    local actual_digest
    local actual_link

    pacman_path=$(command_path pacman)
    bsdtar_path=$(command_path bsdtar)
    gzip_path=$(command_path gzip)
    sha256sum_path=$(command_path sha256sum)
    readlink_path=$(command_path readlink)
    if ! temp_dir=$(mktemp -d "$evidence_dir/.payload.XXXXXX"); then
        printf 'cannot create temporary payload verification directory\n' >&2
        return 1
    fi
    mtree_file=$temp_dir/mtree
    entries_file=$temp_dir/entries.tsv
    artifact_manifest=$temp_dir/artifact-manifest.tsv
    sorted_artifact_manifest=$temp_dir/artifact-manifest.sorted.tsv
    artifact_paths=$temp_dir/artifact-paths
    installed_paths=$temp_dir/installed-paths

    if ! "$bsdtar_path" -xOf "$package_artifact" .MTREE \
        | "$gzip_path" -dc >"$mtree_file"; then
        printf 'cannot read the compressed .MTREE from the exact Arch package artifact\n' >&2
        rm -rf "$temp_dir"
        return 1
    fi
    if ! parse_artifact_mtree "$mtree_file" "$entries_file"; then
        printf 'the Arch package artifact has malformed or incomplete .MTREE metadata\n' >&2
        rm -rf "$temp_dir"
        return 1
    fi

    if ! package_files=$("$pacman_path" -Ql "$package"); then
        printf 'cannot list installed files from package %s\n' "$package" >&2
        rm -rf "$temp_dir"
        return 1
    fi
    if ! awk -v package="$package" '
        NF < 2 || $1 != package { malformed = 1; next }
        {
            path = $0
            sub(/^[^[:space:]]+[[:space:]]+/, "", path)
            if (path != "/") sub(/\/+$/, "", path)
            if (path !~ /^\// || path ~ /[[:space:]]/ || path ~ /(^|\/)\.\.(\/|$)/) malformed = 1
            else print path
            entries++
        }
        END { if (malformed || entries == 0) exit 1 }
    ' <<<"$package_files" | LC_ALL=C sort -u >"$installed_paths"; then
        printf 'installed package file ownership metadata is malformed\n' >&2
        rm -rf "$temp_dir"
        return 1
    fi
    if ! installed_root=$(common_installed_root_prefix "$installed_paths"); then
        printf 'installed package file ownership has no valid common root prefix\n' >&2
        rm -rf "$temp_dir"
        return 1
    fi

    # Concatenate destroot only when it sits outside every artifact logical path.
    # A host prefix such as /usr must not turn logical /bin/voisu into /usr/bin/voisu.
    payload_prefix=
    if [[ -n $installed_root && $installed_root != / ]]; then
        payload_prefix=$installed_root
        while IFS=$'\t' read -r _ mtree_path _ _; do
            if ! logical_path=$(mtree_path_to_logical_path "$mtree_path"); then
                printf 'the Arch package artifact has an unsafe .MTREE path: %s\n' \
                    "$mtree_path" >&2
                rm -rf "$temp_dir"
                return 1
            fi
            if [[ $logical_path == "$installed_root" \
                || $logical_path == "$installed_root"/* ]]; then
                payload_prefix=
                break
            fi
        done <"$entries_file"
    fi

    : >"$artifact_manifest"
    while IFS=$'\t' read -r type mtree_path digest link; do
        if ! logical_path=$(mtree_path_to_logical_path "$mtree_path"); then
            printf 'the Arch package artifact has an unsafe .MTREE path: %s\n' \
                "$mtree_path" >&2
            rm -rf "$temp_dir"
            return 1
        fi
        if ! installed_path=$(awk -v logical="$logical_path" -v prefix="$payload_prefix" '
            $0 == logical {
                print
                matches++
                next
            }
            prefix != "" && prefix != "/" && logical != prefix \
                && index(logical, prefix "/") != 1 && $0 == prefix logical {
                print
                matches++
            }
            END { if (matches != 1) exit 1 }
        ' "$installed_paths"); then
            printf 'installed package is missing the artifact path: %s\n' \
                "$logical_path" >&2
            rm -rf "$temp_dir"
            return 1
        fi
        case "$type" in
            file)
                printf 'file\t%s\t%s\n' "$installed_path" "$digest" >>"$artifact_manifest"
                ;;
            dir)
                printf 'dir\t%s\n' "$installed_path" >>"$artifact_manifest"
                ;;
            link)
                printf 'link\t%s\t%s\n' "$installed_path" "$digest" >>"$artifact_manifest"
                ;;
            *)
                printf 'the Arch package artifact has an unsupported .MTREE entry type\n' >&2
                rm -rf "$temp_dir"
                return 1
                ;;
        esac
    done <"$entries_file"

    LC_ALL=C sort -t $'\t' -k2,2 "$artifact_manifest" >"$sorted_artifact_manifest"
    if awk -F '\t' 'NR > 1 && $2 == previous { exit 1 } { previous = $2 }' \
        "$sorted_artifact_manifest"; then
        :
    else
        printf 'the Arch package artifact has duplicate .MTREE paths\n' >&2
        rm -rf "$temp_dir"
        return 1
    fi
    cut -f2 "$sorted_artifact_manifest" | LC_ALL=C sort -u >"$artifact_paths"
    if ! cmp -s "$artifact_paths" "$installed_paths"; then
        printf 'installed package paths do not match the exact Arch package artifact\n' >&2
        rm -rf "$temp_dir"
        return 1
    fi

    while IFS=$'\t' read -r type installed_path digest link; do
        case "$type" in
            file)
                if [[ ! -f $installed_path || -L $installed_path ]] \
                    || ! actual_digest=$("$sha256sum_path" "$installed_path" | awk '{ print $1 }') \
                    || [[ $actual_digest != "$digest" ]]; then
                    printf 'installed payload content differs from the exact Arch package artifact: %s\n' \
                        "$installed_path" >&2
                    rm -rf "$temp_dir"
                    return 1
                fi
                ;;
            dir)
                if [[ ! -d $installed_path || -L $installed_path ]]; then
                    printf 'installed payload directory differs from the exact Arch package artifact: %s\n' \
                        "$installed_path" >&2
                    rm -rf "$temp_dir"
                    return 1
                fi
                ;;
            link)
                if [[ ! -L $installed_path ]] \
                    || ! actual_link=$("$readlink_path" "$installed_path") \
                    || [[ $actual_link != "$digest" ]]; then
                    printf 'installed payload link differs from the exact Arch package artifact: %s\n' \
                        "$installed_path" >&2
                    rm -rf "$temp_dir"
                    return 1
                fi
                ;;
        esac
    done <"$sorted_artifact_manifest"
    printf 'verified %s installed Arch payload paths against artifact .MTREE\n' \
        "$(wc -l <"$artifact_paths")"
    rm -rf "$temp_dir"
}

if ! current_package=$(read_package_identity); then
    printf '%s\n' 'Hyprland release gate BLOCKED: cannot bind evidence to the exact installed Arch package artifact.' >&2
    exit 4
fi
installed_package=$(pkginfo_value pkgname "$current_package")

if [[ -e $release_file ]]; then
    if [[ ! -f $release_file ]] \
        || ! cmp -s <(printf '%s\n' "$current_package") "$release_file"; then
        printf '%s\n' 'Hyprland release gate BLOCKED: evidence belongs to a different package release.' >&2
        exit 4
    fi
else
    if [[ -e $results_file ]] \
        || compgen -G "$evidence_dir/*.log" >/dev/null \
        || compgen -G "$evidence_dir/*.pass" >/dev/null \
        || compgen -G "$evidence_dir/*.waived" >/dev/null; then
        printf '%s\n' 'Hyprland release gate BLOCKED: initialize a fresh evidence directory before recording manual markers.' >&2
        exit 4
    fi
    printf '%s\n' "$current_package" >"$release_file"
fi

: >"$results_file"

check_packaged_unit() {
    local unit=$1
    local package=$2
    local binary=$3
    local pacman_path
    local systemctl_path
    local package_files
    local expected_fragment
    local expected_binary
    local unit_show
    local fragment_path
    local exec_value
    local exec_path

    pacman_path=$(command_path pacman)
    systemctl_path=$(command_path systemctl)
    if ! package_files=$("$pacman_path" -Ql "$package"); then
        printf 'cannot list files from package %s\n' "$package" >&2
        return 1
    fi
    expected_fragment=$(printf '%s\n' "$package_files" | awk -v basename="$unit" '
        { path = $0; sub(/^[^[:space:]]+[[:space:]]+/, "", path); count = split(path, parts, "/") }
        count > 0 && parts[count] == basename && index(path, "/systemd/user/") { result = path }
        END { if (result != "") print result }
    ')
    expected_binary=$(printf '%s\n' "$package_files" | awk -v basename="$binary" '
        { path = $0; sub(/^[^[:space:]]+[[:space:]]+/, "", path); count = split(path, parts, "/") }
        count > 0 && parts[count] == basename && index(path, "/bin/") { result = path }
        END { if (result != "") print result }
    ')
    if [[ -z $expected_fragment || -z $expected_binary ]]; then
        printf 'package %s does not contain the expected unit or binary\n' "$package" >&2
        return 1
    fi

    if ! unit_show=$("$systemctl_path" --user show "$unit" -p FragmentPath -p ExecStart); then
        printf 'cannot inspect effective unit %s\n' "$unit" >&2
        return 1
    fi
    printf '%s\n' "$unit_show"
    fragment_path=$(awk -F= '$1 == "FragmentPath" { print substr($0, index($0, "=") + 1); exit }' <<<"$unit_show")
    exec_value=$(awk -F= '$1 == "ExecStart" { print substr($0, index($0, "=") + 1); exit }' <<<"$unit_show")
    if [[ -z $fragment_path || -z $exec_value ]]; then
        printf 'unit %s did not report FragmentPath and ExecStart\n' "$unit" >&2
        return 1
    fi
    if [[ ! $exec_value =~ (^|[[:space:];\{])path=([^[:space:];\}]+) ]]; then
        printf 'unit %s has no parseable ExecStart path\n' "$unit" >&2
        return 1
    fi
    exec_path=${BASH_REMATCH[2]}
    printf 'expected_fragment=%s\n' "$expected_fragment"
    printf 'effective_fragment=%s\n' "$fragment_path"
    printf 'expected_binary=%s\n' "$expected_binary"
    printf 'effective_binary=%s\n' "$exec_path"
    if [[ $fragment_path != "$expected_fragment" || $exec_path != "$expected_binary" ]]; then
        printf 'unit %s does not resolve to the packaged unit and binary\n' "$unit" >&2
        return 1
    fi
}

check_packaged_units() {
    local package=$1
    check_packaged_unit voisu.service "$package" voisu-daemon || return 1
    check_packaged_unit voisu-overlay.service "$package" voisu-overlay || return 1
}

validate_service_state() {
    local unit=$1
    local output=$2
    local active_state
    local sub_state

    if ! active_state=$(pkginfo_value ActiveState "$output") \
        || ! sub_state=$(pkginfo_value SubState "$output"); then
        printf '%s did not report ActiveState and SubState\n' "$unit" >&2
        return 1
    fi
    if [[ $active_state != active || $sub_state != running ]]; then
        printf '%s is %s/%s, expected active/running\n' \
            "$unit" "$active_state" "$sub_state" >&2
        return 1
    fi
}

process_has_assignment() {
    local pid=$1
    local name=$2
    local expected=$3
    local environment_file=/proc/$pid/environ

    [[ $pid =~ ^[1-9][0-9]*$ && -r $environment_file ]] || return 1
    tr '\0' '\n' <"$environment_file" | grep -Fqx "$name=$expected"
}

validate_daemon_service() {
    local output=$1
    local main_pid

    validate_service_state voisu.service "$output" || return 1
    if ! main_pid=$(pkginfo_value MainPID "$output"); then
        printf '%s did not report its MainPID\n' voisu.service >&2
        return 1
    fi
    if [[ ! $main_pid =~ ^[1-9][0-9]*$ ]]; then
        printf '%s reported an invalid MainPID: %s\n' voisu.service "$main_pid" >&2
        return 1
    fi
    if [[ ${XDG_SESSION_TYPE-} != wayland \
        || -z ${WAYLAND_DISPLAY-} \
        || -z ${HYPRLAND_INSTANCE_SIGNATURE-} ]]; then
        printf '%s cannot be checked without a complete Hyprland session\n' voisu.service >&2
        return 1
    fi
    if process_has_assignment "$main_pid" WAYLAND_DISPLAY "$WAYLAND_DISPLAY" \
        && process_has_assignment "$main_pid" XDG_SESSION_TYPE wayland \
        && process_has_assignment "$main_pid" \
            HYPRLAND_INSTANCE_SIGNATURE "$HYPRLAND_INSTANCE_SIGNATURE"; then
        return 0
    fi
    printf '%s process does not carry the active Wayland session environment\n' voisu.service >&2
    return 1
}

validate_overlay_service() {
    local output=$1
    validate_service_state voisu-overlay.service "$output"
}

run_service_probe() {
    local name=$1
    local validator=$2
    shift 2
    local log="$evidence_dir/$name.log"
    local output
    local status
    {
        printf '$'
        printf ' %q' "$@"
        printf '\n'
    } >"$log"

    set +e
    output=$("$@" 2>&1)
    status=$?
    set -e
    printf '%s\n' "$output" >>"$log"
    printf 'exit=%s\n' "$status" >>"$log"
    cat "$log"
    if ((status != 0)); then
        record_result "$name" FAIL "exit=$status"
    elif ! "$validator" "$output"; then
        record_result "$name" FAIL 'unexpected service state or environment'
    else
        record_result "$name" PASS 'active/running with expected environment'
    fi
}

journal_has_entries() {
    local output=$1
    awk '
        /^[[:space:]]*$/ { next }
        /^[[:space:]]*-- No entries --[[:space:]]*$/ { next }
        /^[[:space:]]*(Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)[[:space:]]+[0-9][0-9]?[[:space:]]+[0-9][0-9]:[0-9][0-9]:[0-9][0-9][[:space:]]+[^[:space:]]+[[:space:]]+[^[:space:]:]+(\[[0-9]+\])?:[[:space:]]/ {
            found = 1
        }
        END { exit !found }
    ' <<<"$output"
}

run_journal_probe() {
    local name=$1
    shift
    local log="$evidence_dir/$name.log"
    local stdout_file="$log.stdout"
    local stderr_file="$log.stderr"
    local output
    local diagnostics
    local status
    {
        printf '$'
        printf ' %q' "$@"
        printf '\n'
    } >"$log"

    set +e
    "$@" >"$stdout_file" 2>"$stderr_file"
    status=$?
    set -e
    output=$(<"$stdout_file")
    diagnostics=$(<"$stderr_file")
    cat "$stdout_file" >>"$log"
    if [[ -n $diagnostics ]]; then
        printf '%s\n' 'stderr:' >>"$log"
        cat "$stderr_file" >>"$log"
    fi
    printf 'exit=%s\n' "$status" >>"$log"
    cat "$log"
    rm -f "$stdout_file" "$stderr_file"
    if ((status != 0)); then
        record_result "$name" FAIL "exit=$status"
    elif [[ $diagnostics == *[![:space:]]* ]]; then
        record_result "$name" FAIL 'journal diagnostics'
    elif ! journal_has_entries "$output"; then
        record_result "$name" FAIL 'no journal entries'
    else
        record_result "$name" PASS 'non-empty journal'
    fi
}

run_session_probe
run_probe voisu-version "$(command_path voisu)" --version
run_probe voisu-daemon-version "$(command_path voisu-daemon)" --version
run_probe package-info "$(command_path pacman)" -Qi "$installed_package"
run_probe payload-content verify_installed_payload "$installed_package"
run_probe user-units "$(command_path systemd-analyze)" --user verify voisu.service voisu-overlay.service
run_probe packaged-units check_packaged_units "$installed_package"
run_service_probe daemon-service validate_daemon_service "$(command_path systemctl)" --user show voisu.service \
    -p ActiveState -p SubState -p MainPID -p Environment
run_service_probe overlay-service validate_overlay_service "$(command_path systemctl)" --user show voisu-overlay.service \
    -p ActiveState -p SubState -p MainPID
run_probe hyprland-version "$(command_path hyprctl)" version
run_probe hyprland-bindings "$(command_path hyprctl)" binds -j
run_probe doctor "$(command_path voisu)" doctor --verbose
run_journal_probe daemon-journal "$(command_path journalctl)" --user -u voisu.service -n 200 --no-pager
run_journal_probe overlay-journal "$(command_path journalctl)" --user -u voisu-overlay.service -n 200 --no-pager

pending=0
for gate in "${MANUAL_GATES[@]}"; do
    pass_marker="$evidence_dir/$gate.pass"
    waive_marker="$evidence_dir/$gate.waived"
    if [[ -f $pass_marker ]]; then
        record_result "$gate" PASS "$(basename "$pass_marker")"
    elif [[ -f $waive_marker ]] && grep -q '[^[:space:]]' "$waive_marker"; then
        record_result "$gate" WAIVED "$(basename "$waive_marker")"
    else
        record_result "$gate" PENDING "create .pass or non-whitespace .waived marker"
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
