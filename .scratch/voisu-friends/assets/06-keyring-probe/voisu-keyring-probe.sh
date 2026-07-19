#!/usr/bin/env bash
# voisu-keyring-probe.sh
#
# Ticket 06 follow-up: run this as an ExecStartPre of voisu.service (via the
# keyring-probe.conf drop-in) to empirically confirm, across an actual reboot,
# whether org.freedesktop.secrets is reachable/unlocked at the moment the
# service starts. Every D-Bus/secret-tool call is wrapped in `timeout 10` so
# a stuck prompt or slow activation cannot hang the unit's start-up.
#
# Output goes to the journal via systemd-cat so `journalctl --user -t
# voisu-keyring-probe` shows one line per check, tagged with epoch-ms.
#
# This script only READS/WRITES a disposable probe secret (label
# voisu-probe, attributes: service=voisu-probe ticket=06) and always deletes
# it at the end, even on failure paths. It never touches real credentials.

set -u

TAG="voisu-keyring-probe"
LOG() {
    # $1 = message; emitted as a single journal line via systemd-cat
    printf '%s\n' "$1" | systemd-cat -t "$TAG"
}

now_ms() { date +%s%3N; }

LOG "start epoch_ms=$(now_ms)"

# 1. Is org.freedesktop.secrets currently owned (i.e. Secret Service is up),
#    or only activatable?
OWNER=$(timeout 10 busctl --user call org.freedesktop.DBus /org/freedesktop/DBus \
    org.freedesktop.DBus GetNameOwner s org.freedesktop.secrets 2>&1)
OWNER_RC=$?
if [ $OWNER_RC -eq 0 ]; then
    LOG "epoch_ms=$(now_ms) secrets_owned=yes owner=${OWNER}"
else
    LOG "epoch_ms=$(now_ms) secrets_owned=no detail=${OWNER} rc=${OWNER_RC}"
fi

# 2. Does the default collection exist, and is it locked? (This call itself
#    may trigger D-Bus activation of the secrets service if nothing owns the
#    name yet — that is expected and part of what we're measuring.)
DEFAULT_PATH=$(timeout 10 busctl --user call org.freedesktop.secrets \
    /org/freedesktop/secrets org.freedesktop.Secret.Service ReadAlias s default 2>&1)
DEFAULT_RC=$?
if [ $DEFAULT_RC -eq 0 ]; then
    LOG "epoch_ms=$(now_ms) default_collection=present path=${DEFAULT_PATH}"
else
    LOG "epoch_ms=$(now_ms) default_collection=would_block_or_error detail=${DEFAULT_PATH} rc=${DEFAULT_RC}"
fi

LOCKED=$(timeout 10 busctl --user get-property org.freedesktop.secrets \
    /org/freedesktop/secrets/aliases/default org.freedesktop.Secret.Collection Locked 2>&1)
LOCKED_RC=$?
if [ $LOCKED_RC -eq 0 ]; then
    LOG "epoch_ms=$(now_ms) locked_property=${LOCKED}"
else
    LOG "epoch_ms=$(now_ms) locked_property=would_block_or_error detail=${LOCKED} rc=${LOCKED_RC}"
fi

# 3. Full round trip using secret-tool (store -> lookup -> delete), each step
#    timed and timeout-guarded. Uses a disposable, clearly-labeled attribute
#    pair so it can never collide with real application secrets.
STORE_START=$(now_ms)
STORE_OUT=$(printf '%s' "voisu-probe-disposable-value" | timeout 10 secret-tool store \
    --label="voisu-probe" service voisu-probe ticket 06 2>&1)
STORE_RC=$?
STORE_END=$(now_ms)
LOG "epoch_ms=${STORE_END} step=store rc=${STORE_RC} ms=$((STORE_END - STORE_START)) detail=${STORE_OUT}"

LOOKUP_START=$(now_ms)
LOOKUP_OUT=$(timeout 10 secret-tool lookup service voisu-probe ticket 06 2>&1)
LOOKUP_RC=$?
LOOKUP_END=$(now_ms)
if [ "$LOOKUP_OUT" = "voisu-probe-disposable-value" ]; then
    LOOKUP_RESULT="match"
else
    LOOKUP_RESULT="mismatch_or_empty"
fi
LOG "epoch_ms=${LOOKUP_END} step=lookup rc=${LOOKUP_RC} ms=$((LOOKUP_END - LOOKUP_START)) result=${LOOKUP_RESULT}"

DELETE_START=$(now_ms)
DELETE_OUT=$(timeout 10 secret-tool clear service voisu-probe ticket 06 2>&1)
DELETE_RC=$?
DELETE_END=$(now_ms)
LOG "epoch_ms=${DELETE_END} step=delete rc=${DELETE_RC} ms=$((DELETE_END - DELETE_START)) detail=${DELETE_OUT}"

LOG "end epoch_ms=$(now_ms) summary owner_rc=${OWNER_RC} default_rc=${DEFAULT_RC} locked_rc=${LOCKED_RC} store_rc=${STORE_RC} lookup_rc=${LOOKUP_RC} delete_rc=${DELETE_RC}"

exit 0
