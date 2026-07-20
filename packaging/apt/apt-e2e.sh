#!/usr/bin/env bash
set -euo pipefail

# End-to-end verification of the Voisu apt channel (ticket 13, GH issue #45).
#
# Proves the whole friend journey against a REAL .deb built by
# packaging/build-deb.sh and a REAL signed repo produced by make-apt-repo.sh,
# with signatures actually enforced (no --allow-unauthenticated, no trusted=yes):
#
#   1. build a Voisu .deb (v1) on Ubuntu, plus a strictly-newer .deb (v2) from
#      one extra commit, using packaging/build-deb.sh unchanged;
#   2. publish v1 into a local repo with an EPHEMERAL throwaway gpg key;
#   3. serve it over local HTTP and, in the same fresh Ubuntu userland, add the
#      repo exactly the way packaging/apt/README.md documents;
#   4. apt-get update + install voisu -> capture the installed version;
#   5. prove verification is enforced: a bogus keyring makes apt-get update fail;
#   6. republish v2 (idempotent), apt-get update + upgrade -> capture the newer
#      version and assert dpkg sees it as strictly greater.
#
# WHY the image choices. build-deb.sh's `$auto` deps come from dpkg-shlibdeps,
# which must run on the target distro, so everything happens in an Ubuntu
# container. We use ubuntu:rolling (currently 26.04 LTS "resolute"): it has live
# mirrors AND packages gtk4-layer-shell, which the Overlay links. 24.10
# (oracular, what CI targets) is EOL and off the primary mirrors, and 24.04 LTS
# does NOT package gtk4-layer-shell at all, so the Overlay's runtime dependency
# would be unsatisfiable there (an open risk for ticket 14 -- see the PR). Voisu's
# crate graph is edition-2024 (rustc >= 1.85), newer than the distro's apt rustc,
# so we install a stable toolchain via rustup. None of this touches the packaging
# scripts under test.
#
# FOREGROUND ONLY. Every container runs to completion in a single foreground
# `podman run`; the local HTTP server is a `&` job INSIDE the friend container's
# script, which tears it down before the container exits. Heavy compiles are
# resumable: CARGO_HOME and the target dir live on a host scratch mount, so a
# re-invocation of the `debs` phase picks up where a timed-out one left off.
#
# Usage:  packaging/apt/apt-e2e.sh [image|debs|friend|all]   (default: all)
#   VOISU_APT_E2E_SCRATCH   host scratch dir (default /var/tmp/voisu-apt-e2e)
#   VOISU_CONTAINER         container CLI (default podman)

phase=${1:-all}
case "$phase" in image|debs|friend|all) : ;; *)
    printf 'usage: %s [image|debs|friend|all]\n' "$(basename "$0")" >&2
    exit 1 ;;
esac

engine=${VOISU_CONTAINER:-podman}
if ! command -v "$engine" >/dev/null 2>&1; then
    printf 'container engine %s not found\n' "$engine" >&2
    exit 1
fi

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(realpath "$(git -C "$script_dir" rev-parse --show-toplevel)")

base_image=ubuntu:rolling
image=voisu-apt-e2e:rolling
scratch=${VOISU_APT_E2E_SCRATCH:-/var/tmp/voisu-apt-e2e}
mkdir -p "$scratch"/{out/v1,out/v2,cargo,target}

# --- inner script: build the toolchain image -------------------------------
build_image() {
    printf '=== [image] building %s (base %s) ===\n' "$image" "$base_image"
    cat > "$scratch/Containerfile" <<'CONTAINERFILE'
FROM ubuntu:rolling
ENV DEBIAN_FRONTEND=noninteractive
# Enable universe (gtk4-layer-shell etc.) and install the build + publish toolchain.
RUN sed -i 's/^\(Components: .*\)$/\1 universe multiverse/' /etc/apt/sources.list.d/ubuntu.sources \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates curl git build-essential pkg-config \
        libgtk-4-dev libgtk4-layer-shell-dev libxkbcommon-dev \
        dpkg-dev apt-utils gnupg python3 xz-utils \
    && rm -rf /var/lib/apt/lists/*
# rustup stable (>= 1.85 for edition 2024); cargo-deb pinned to build-deb's version.
ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:/usr/local/bin:/usr/bin:/bin
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --default-toolchain stable --profile minimal \
    && cargo install cargo-deb --version 3.7.0 --locked --root /usr/local
CONTAINERFILE
    "$engine" build -t "$image" -f "$scratch/Containerfile" "$scratch"
}

# --- inner script: build v1 and v2 debs ------------------------------------
build_debs() {
    printf '=== [debs] building v1 + v2 .debs on %s ===\n' "$base_image"
    cat > "$scratch/builder.sh" <<'BUILDER'
set -euo pipefail
export TMPDIR=/var/tmp
# Clone the mounted (committed) tree into a writable checkout; symlink the target
# dir onto the host cache so cargo-deb's literal `target/release/` assets resolve
# through it AND compilation stays incremental across re-runs.
rm -rf /work
git clone /src /work
mkdir -p /cache/target
ln -s /cache/target /work/target
# .gitignore only excludes a `target/` DIRECTORY; our cache symlink is a symlink,
# so exclude it locally (untracked, keeps build-deb's clean-tree check happy).
echo 'target' >> /work/.git/info/exclude
cd /work
git config user.email e2e@example.invalid
git config user.name 'Voisu E2E'

echo '--- build v1 ---'
packaging/build-deb.sh
mkdir -p /out/v1 && rm -f /out/v1/*.deb
cp dist/deb/*.deb /out/v1/

# One extra (empty) commit -> higher commit count -> strictly-newer dev version.
git commit --allow-empty -m 'e2e: bump for upgrade test'
echo '--- build v2 ---'
packaging/build-deb.sh
mkdir -p /out/v2 && rm -f /out/v2/*.deb
cp dist/deb/*.deb /out/v2/

echo '--- artifacts ---'
basename /out/v1/*.deb
basename /out/v2/*.deb
BUILDER
    "$engine" run --rm --security-opt label=disable \
        -v "$repo_root:/src:ro" \
        -v "$scratch/out:/out" \
        -v "$scratch/cargo:/opt/cargo-cache" \
        -v "$scratch/target:/cache/target" \
        -v "$scratch/builder.sh:/opt/builder.sh:ro" \
        -e CARGO_HOME=/opt/cargo-cache \
        "$image" bash /opt/builder.sh
}

# --- inner script: publish, serve, install, upgrade ------------------------
run_friend() {
    printf '=== [friend] publish -> add repo -> install -> upgrade ===\n'
    if ! ls "$scratch"/out/v1/*.deb >/dev/null 2>&1 || ! ls "$scratch"/out/v2/*.deb >/dev/null 2>&1; then
        printf 'missing built debs in %s/out; run the debs phase first\n' "$scratch" >&2
        exit 1
    fi
    cat > "$scratch/friend.sh" <<'FRIEND'
set -euo pipefail
exec > >(tee /out/evidence.txt) 2>&1

# --- an EPHEMERAL, passphrase-less signing key (destroyed with the container) --
export GNUPGHOME=$(mktemp -d)
chmod 700 "$GNUPGHOME"
cat > "$GNUPGHOME/keyspec" <<'KEY'
%no-protection
Key-Type: eddsa
Key-Curve: ed25519
Key-Usage: sign
Name-Real: Voisu E2E Throwaway
Name-Email: e2e@example.invalid
Expire-Date: 0
%commit
KEY
gpg --batch --gen-key "$GNUPGHOME/keyspec" 2>/dev/null
keyid=$(gpg --list-secret-keys --with-colons | awk -F: '/^fpr:/{print $10; exit}')
echo "[evidence] ephemeral signing key fingerprint: $keyid"

repo=/srv/voisu-apt
rm -rf "$repo"; mkdir -p /srv

echo '=== publish v1 ==='
v1deb=$(ls /out/v1/*.deb)
echo "[evidence] v1 deb: $(basename "$v1deb")"
VOISU_APT_GPG_KEY="$keyid" /opt/make-apt-repo.sh "$repo" "$v1deb"

echo '=== serve repo over local HTTP ==='
( cd "$repo" && python3 -m http.server 8099 --bind 127.0.0.1 ) >/tmp/http.log 2>&1 &
http_pid=$!
trap 'kill "$http_pid" 2>/dev/null || true' EXIT
for i in $(seq 1 20); do
    curl -fsS http://127.0.0.1:8099/dists/stable/InRelease >/dev/null 2>&1 && break
    sleep 0.5
done

echo '=== add repo the DOCUMENTED way (signed-by keyring, no trusted=yes) ==='
mkdir -p /usr/share/keyrings
curl -fsSL http://127.0.0.1:8099/voisu-archive-keyring.asc \
    | gpg --dearmor -o /usr/share/keyrings/voisu-archive-keyring.gpg
echo 'deb [signed-by=/usr/share/keyrings/voisu-archive-keyring.gpg arch=amd64] http://127.0.0.1:8099 stable main' \
    > /etc/apt/sources.list.d/voisu.list
cat /etc/apt/sources.list.d/voisu.list

echo '=== apt-get update (full: needs ubuntu deps too) ==='
apt-get update

echo '=== signature-enforcement NEGATIVE test (a WRONG key must be rejected) ==='
# Swap in an unrelated valid key: the signed-by source must then fail to verify.
# apt-get update EXITS 0 even on a verification failure (it just warns and keeps
# the old index), so we assert on the emitted error, not the exit code.
wrong_home=$(mktemp -d); chmod 700 "$wrong_home"
GNUPGHOME="$wrong_home" gpg --batch --gen-key "$GNUPGHOME/keyspec" 2>/dev/null
GNUPGHOME="$wrong_home" gpg --batch --yes --export -o /usr/share/keyrings/voisu-archive-keyring.gpg
apt-get update -o Dir::Etc::sourcelist=/etc/apt/sources.list.d/voisu.list \
    -o Dir::Etc::sourceparts=/dev/null -o APT::Get::List-Cleanup=0 2>&1 | tee /tmp/neg.log || true
if grep -Eqi 'NO_PUBKEY|not signed|signature|not updated' /tmp/neg.log; then
    echo '[evidence] apt REFUSED the repo under the wrong key:'
    grep -Ei 'NO_PUBKEY|not signed|verification|not updated' /tmp/neg.log | head -2
else
    echo 'FAIL: a wrong signing key did not trigger a verification failure'; exit 1
fi
# restore the real (ephemeral) key and refresh
curl -fsSL http://127.0.0.1:8099/voisu-archive-keyring.asc \
    | gpg --dearmor --yes -o /usr/share/keyrings/voisu-archive-keyring.gpg
apt-get update >/dev/null

echo '=== install voisu (signature verified) ==='
DEBIAN_FRONTEND=noninteractive apt-get install -y voisu
v1ver=$(dpkg-query -W -f='${Version}' voisu)
echo "[evidence] installed version (v1): $v1ver"

echo '=== republish a strictly-newer v2 (idempotent) ==='
v2deb=$(ls /out/v2/*.deb)
echo "[evidence] v2 deb: $(basename "$v2deb")"
VOISU_APT_GPG_KEY="$keyid" /opt/make-apt-repo.sh "$repo" "$v2deb"
echo '[evidence] pool after republish:'
ls -1 "$repo/pool/main/v/voisu"

echo '=== apt-get update + upgrade picks up v2 ==='
apt-get update
DEBIAN_FRONTEND=noninteractive apt-get upgrade -y
v2ver=$(dpkg-query -W -f='${Version}' voisu)
echo "[evidence] installed version after upgrade (v2): $v2ver"

echo '=== assert v2 > v1 ==='
if [ "$v1ver" = "$v2ver" ]; then
    echo "FAIL: version did not change on upgrade ($v1ver)"; exit 1
fi
if ! dpkg --compare-versions "$v2ver" gt "$v1ver"; then
    echo "FAIL: $v2ver is not greater than $v1ver"; exit 1
fi
echo "[evidence] PASS: upgraded $v1ver -> $v2ver with signature verification enforced"
FRIEND
    "$engine" run --rm --security-opt label=disable \
        -v "$scratch/out:/out" \
        -v "$repo_root/packaging/apt/make-apt-repo.sh:/opt/make-apt-repo.sh:ro" \
        -v "$scratch/friend.sh:/opt/friend.sh:ro" \
        "$image" bash /opt/friend.sh
}

case "$phase" in
    image)  build_image ;;
    debs)   build_debs ;;
    friend) run_friend ;;
    all)    build_image; build_debs; run_friend ;;
esac
