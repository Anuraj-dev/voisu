#!/usr/bin/env bash
set -euo pipefail

# Ubuntu 26.04 install-smoke for the release gate (ticket 14).
#
# Installs the freshly-built release .deb through the REAL apt path: it is
# published into a throwaway, locally-served apt repo signed with an EPHEMERAL
# key (the live gh-pages channel does not exist yet at gate time, because
# publishing is gated on THIS job), then added and installed exactly the way a
# friend does per packaging/apt/README.md, with signatures enforced (no
# --allow-unauthenticated, no trusted=yes). This also exercises
# packaging/apt/make-apt-repo.sh end-to-end before any real publish. The pattern
# is cribbed from packaging/apt/apt-e2e.sh's friend phase, minus the build/GPG
# negative tests that apt-e2e already owns.
#
# Runs as root inside an ubuntu:26.04 container. A container has no systemd user
# session, so enabling/starting the user services is out of scope here (that is
# the ticket 15 live-desktop smoke); unit-file correctness is checked statically
# with `systemd-analyze verify`, which needs no session.
#
# Usage: smoke-ubuntu.sh /path/to/voisu_<ver>_amd64.deb

deb=${1:?usage: smoke-ubuntu.sh <deb>}
test -r "$deb"

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$script_dir/../.." && pwd)

export DEBIAN_FRONTEND=noninteractive
# universe/multiverse so gtk4-layer-shell (an install-time runtime dep of the
# Overlay) resolves.
sed -i 's/^\(Components: .*\)$/\1 universe multiverse/' /etc/apt/sources.list.d/ubuntu.sources
apt-get update
apt-get install -y --no-install-recommends \
    apt-utils dpkg-dev gnupg gpgv coreutils util-linux gzip \
    ca-certificates curl python3 lintian systemd

# --- an EPHEMERAL, passphrase-less signing key (destroyed with the container) --
export GNUPGHOME=$(mktemp -d); chmod 700 "$GNUPGHOME"
cat > "$GNUPGHOME/keyspec" <<'KEY'
%no-protection
Key-Type: eddsa
Key-Curve: ed25519
Key-Usage: sign
Name-Real: Voisu Release Smoke
Name-Email: smoke@example.invalid
Expire-Date: 0
%commit
KEY
gpg --batch --gen-key "$GNUPGHOME/keyspec" 2>/dev/null
keyfpr=$(gpg --list-secret-keys --with-colons | awk -F: '/^fpr:/{print $10; exit}')

# --- publish the release deb into a local staging repo ---
repo=/srv/voisu-apt; rm -rf "$repo"; mkdir -p "$repo"
echo "== publish $(basename "$deb") with the ephemeral key =="
VOISU_APT_GPG_KEY="$keyfpr" "$repo_root/packaging/apt/make-apt-repo.sh" "$repo" "$deb"

# --- serve over local HTTP (foreground job, torn down on exit) ---
( cd "$repo" && python3 -m http.server 8099 --bind 127.0.0.1 ) >/tmp/http.log 2>&1 &
http_pid=$!
trap 'kill "$http_pid" 2>/dev/null || true' EXIT
for _ in $(seq 1 20); do
    curl -fsS http://127.0.0.1:8099/dists/stable/InRelease >/dev/null 2>&1 && break
    sleep 0.5
done

# --- add the repo the DOCUMENTED way (fingerprint-pinned, signed-by) ---
echo "== add the repo (fingerprint-pin -> signed-by) =="
tmp=$(mktemp -d)
curl -fsSL http://127.0.0.1:8099/voisu-archive-keyring.asc -o "$tmp/key.asc"
npub=$(gpg --show-keys --with-colons "$tmp/key.asc" | grep -c '^pub:' || true)
test "$npub" = 1 || { echo "FAIL: served bundle has $npub primary keys (expected 1)"; exit 1; }
got=$(gpg --show-keys --with-colons "$tmp/key.asc" | awk -F: '$1=="pub"{p=1;next} p&&$1=="fpr"{print $10;exit}')
test "$got" = "$keyfpr" || { echo "FAIL: served fingerprint $got != publisher $keyfpr"; exit 1; }
install -d -m 0755 /etc/apt/keyrings
gpg --dearmor < "$tmp/key.asc" > "$tmp/voisu.gpg"
install -m 0644 "$tmp/voisu.gpg" /etc/apt/keyrings/voisu-archive-keyring.gpg
echo 'deb [signed-by=/etc/apt/keyrings/voisu-archive-keyring.gpg arch=amd64] http://127.0.0.1:8099 stable main' \
    > /etc/apt/sources.list.d/voisu.list

echo "== apt-get update + install voisu (signature verified) =="
apt-get update
apt-get install -y voisu

# --- assertions: the installed binaries run ---
echo "== binaries =="
test -x /usr/bin/voisu
test -x /usr/bin/voisu-daemon
test -x /usr/bin/voisu-overlay
voisu --version
voisu-daemon --help >/dev/null

# --- assertions: both shipped user units statically verify ---
echo "== systemd-analyze verify (both user units) =="
systemd-analyze verify /usr/lib/systemd/user/voisu.service
systemd-analyze verify /usr/lib/systemd/user/voisu-overlay.service

# --- cheap --refresh regression: re-sign the staged repo, prove pool bytes are
#     immutable and apt still updates/installs against the refreshed metadata ---
echo "== make-apt-repo.sh --refresh (metadata re-sign, pool immutable) =="
before=$(cd "$repo/pool/main/v/voisu" && sha256sum ./* | sort)
VOISU_APT_GPG_KEY="$keyfpr" "$repo_root/packaging/apt/make-apt-repo.sh" --refresh "$repo"
after=$(cd "$repo/pool/main/v/voisu" && sha256sum ./* | sort)
test "$before" = "$after" || { echo "FAIL: --refresh mutated pool bytes"; exit 1; }
# the refreshed metadata is still a valid, signed repo apt can consume
apt-get update
apt-get install --reinstall -y voisu
echo "[evidence] --refresh kept pool bytes and apt re-installed against refreshed metadata"

# --- lintian on the .deb (clean-enough) ---
# The three suppressed tags are the ones documented in
# packaging/deb/lintian-overrides:
#   maintainer-script-calls-systemctl  - false positive: the maintainer scripts
#       only PRINT `systemctl --user ...` guidance, never call it.
#   no-manual-page                     - the binaries expose built-in --help.
#   initial-upload-closes-no-bugs      - self-hosted repo, not the Debian archive.
# Any OTHER error/warning fails the gate.
echo "== lintian =="
lintian --fail-on error,warning \
    --suppress-tags maintainer-script-calls-systemctl,no-manual-page,initial-upload-closes-no-bugs \
    "$deb"

echo "PASS: ubuntu:26.04 install-smoke (apt path + binaries + unit verify + lintian)"
