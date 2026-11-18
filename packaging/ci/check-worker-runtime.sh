#!/usr/bin/env bash
# Checks one whisper.cpp worker directory: voisu-local-asr carries the
# RUNPATH $ORIGIN, voisu-whisper-runtime.txt sits beside it, and every file
# that list names exists in the same directory. Packaging runs it on what it
# ships; voisu-local-asr's tests run it on the cargo build.
#
# usage: check-worker-runtime.sh <dir>
set -euo pipefail

dir=${1:?usage: check-worker-runtime.sh <dir>}
worker="$dir/voisu-local-asr"
list="$dir/voisu-whisper-runtime.txt"

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

[ -x "$worker" ] || fail "$worker is missing or not executable"
[ -f "$list" ] || fail "$list is missing"
# RUNPATH (not RPATH): LD_LIBRARY_PATH must not outrank the private libraries.
runpath=$(readelf -d "$worker" | sed -n 's/.*(RUNPATH).*\[\(.*\)\]$/\1/p')
[ "$runpath" = '$ORIGIN' ] || fail "$worker RUNPATH is '${runpath}', not \$ORIGIN"
[ -s "$list" ] || fail "$list is empty"
count=0
while read -r lib; do
    case $lib in
        ''|*/*) fail "$list names '$lib', not a file in this directory" ;;
    esac
    [ -f "$dir/$lib" ] || fail "$dir/$lib is listed but missing"
    count=$((count + 1))
done < "$list"
echo "ok: $worker with RUNPATH \$ORIGIN and $count runtime files"
