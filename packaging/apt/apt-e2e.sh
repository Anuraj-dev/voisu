#!/usr/bin/env bash
set -euo pipefail
umask 077

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
#      repo exactly the way packaging/apt/README.md documents (fetch key ->
#      fingerprint-pin -> /etc/apt/keyrings -> signed-by);
#   4. apt-get update + install voisu -> capture the installed version;
#   5. prove verification is enforced: a WRONG key makes apt-get update fail;
#   6. prove the detached Release.gpg path also authenticates (InRelease removed);
#   7. republish v2 (idempotent), apt-get update + --only-upgrade voisu ->
#      capture the newer version and assert dpkg sees it as strictly greater.
#
# REPRODUCIBILITY (review round 1). The harness refuses a dirty checkout, records
# HEAD, and derives every script under test FROM that commit (make-apt-repo.sh is
# extracted via `git show`, and build-deb.sh runs inside a fresh `git clone` of
# the committed tree) -- nothing is taken from the live working tree. The scratch
# area is namespaced by commit, created 0700 and owned-by-us (rejecting symlinks
# or foreign-owned dirs), and a manifest (commit, image, script + artifact
# hashes) ties the phased runs together: `friend` refuses to run unless the
# manifest matches the current commit with exactly one v1 and one v2 package.
#
# WHY the image. build-deb.sh's `$auto` deps come from dpkg-shlibdeps, which must
# run on the target distro, so everything happens in an Ubuntu container. We pin
# ubuntu:26.04 (LTS "resolute") -- a specific release, NOT the moving `rolling`
# tag -- so the encoded dependency floors don't drift. It has live mirrors and
# packages gtk4-layer-shell (which the Overlay links); 24.10 is EOL and 24.04
# lacks gtk4-layer-shell. Voisu is edition-2024 (rustc >= 1.85), newer than the
# distro apt rustc, so we install a stable toolchain via rustup.
#
# FOREGROUND ONLY. Every container runs to completion in a single foreground
# `podman run`; the local HTTP server is a `&` job INSIDE the friend container's
# script, which tears it down before the container exits. Heavy compiles are
# resumable: CARGO_HOME and the target dir live on the per-commit scratch mount.
#
# Usage:  packaging/apt/apt-e2e.sh [image|debs|friend|all]   (default: all)
#   VOISU_APT_E2E_SCRATCH   host scratch base (default /var/tmp/voisu-apt-e2e)
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

base_image=ubuntu:26.04
image=voisu-apt-e2e:26.04
cache_base=${VOISU_APT_E2E_SCRATCH:-/var/tmp/voisu-apt-e2e}

# --- secure a cache directory (finding 7) ----------------------------------
# Reject symlinks and foreign-owned dirs before writing predictable filenames;
# create 0700 owned by us. umask 077 (top of file) keeps created files private.
secure_dir() {
    local d=$1
    if test -L "$d"; then
        printf 'refusing: %s is a symlink\n' "$d" >&2; exit 1
    fi
    mkdir -p "$d"
    chmod 700 "$d"
    if test "$(stat -c %u "$d")" != "$(id -u)"; then
        printf 'refusing: %s is not owned by the current user\n' "$d" >&2; exit 1
    fi
}
secure_dir "$cache_base"

# --- clean-checkout + commit pin for the artifact phases (finding 6) -------
commit=""
scratch=""
if test "$phase" != image; then
    if test -n "$(git -C "$repo_root" status --porcelain)"; then
        printf 'refusing: working tree is dirty; commit first so the e2e tests an exact commit\n' >&2
        exit 1
    fi
    commit=$(git -C "$repo_root" rev-parse HEAD)
    scratch="$cache_base/$commit"
    secure_dir "$scratch"
    mkdir -p "$scratch"/{out/v1,out/v2,cargo,target}
    # Scripts under test come from the committed tree, not the live checkout.
    git -C "$repo_root" show "$commit:packaging/apt/make-apt-repo.sh" > "$scratch/make-apt-repo.sh"
    manifest="$scratch/manifest.txt"
fi

sha256_of() { sha256sum "$1" | awk '{print $1}'; }

# --- inner script: build the toolchain image -------------------------------
build_image() {
    printf '=== [image] building %s (base %s) ===\n' "$image" "$base_image"
    cat > "$cache_base/Containerfile" <<'CONTAINERFILE'
FROM ubuntu:26.04
ENV DEBIAN_FRONTEND=noninteractive
# Enable universe (gtk4-layer-shell etc.) and install the build + publish toolchain.
RUN sed -i 's/^\(Components: .*\)$/\1 universe multiverse/' /etc/apt/sources.list.d/ubuntu.sources \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates curl git build-essential pkg-config \
        libgtk-4-dev libgtk4-layer-shell-dev libxkbcommon-dev \
        dpkg-dev apt-utils gnupg gpgv python3 xz-utils \
    && rm -rf /var/lib/apt/lists/*
# rustup stable (>= 1.85 for edition 2024); cargo-deb pinned to build-deb's version.
ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:/usr/local/bin:/usr/bin:/bin
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --default-toolchain stable --profile minimal \
    && cargo install cargo-deb --version 3.7.0 --locked --root /usr/local
CONTAINERFILE
    "$engine" build -t "$image" -f "$cache_base/Containerfile" "$cache_base"
}

# --- inner script: build v1 and v2 debs ------------------------------------
build_debs() {
    printf '=== [debs] building v1 + v2 .debs on %s (commit %s) ===\n' "$base_image" "$commit"
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

    # Manifest: pin commit, image, and artifact/script hashes (finding 6).
    local v1 v2
    v1=$(ls "$scratch"/out/v1/*.deb) ; v2=$(ls "$scratch"/out/v2/*.deb)
    {
        printf 'commit=%s\n' "$commit"
        printf 'image=%s\n' "$base_image"
        printf 'make_apt_repo_sha256=%s\n' "$(sha256_of "$scratch/make-apt-repo.sh")"
        printf 'v1_deb=%s\n' "$(basename "$v1")"
        printf 'v1_sha256=%s\n' "$(sha256_of "$v1")"
        printf 'v2_deb=%s\n' "$(basename "$v2")"
        printf 'v2_sha256=%s\n' "$(sha256_of "$v2")"
    } > "$manifest"
    printf 'manifest written: %s\n' "$manifest"
}

# --- verify the manifest matches this commit + exactly one v1/v2 (finding 6) -
verify_manifest() {
    if ! test -f "$manifest"; then
        printf 'no manifest at %s; run the debs phase first\n' "$manifest" >&2; exit 1
    fi
    local m_commit; m_commit=$(awk -F= '/^commit=/{print $2}' "$manifest")
    if test "$m_commit" != "$commit"; then
        printf 'manifest commit %s != current HEAD %s; rebuild the debs\n' "$m_commit" "$commit" >&2; exit 1
    fi
    local n1 n2
    n1=$(ls "$scratch"/out/v1/*.deb 2>/dev/null | wc -l)
    n2=$(ls "$scratch"/out/v2/*.deb 2>/dev/null | wc -l)
    if test "$n1" -ne 1 || test "$n2" -ne 1; then
        printf 'expected exactly one v1 and one v2 package (got %s/%s)\n' "$n1" "$n2" >&2; exit 1
    fi
    local v1 v2
    v1=$(ls "$scratch"/out/v1/*.deb) ; v2=$(ls "$scratch"/out/v2/*.deb)
    if test "$(sha256_of "$v1")" != "$(awk -F= '/^v1_sha256=/{print $2}' "$manifest")" \
        || test "$(sha256_of "$v2")" != "$(awk -F= '/^v2_sha256=/{print $2}' "$manifest")"; then
        printf 'artifact hashes do not match the manifest; rebuild the debs\n' >&2; exit 1
    fi
    if test "$(sha256_of "$scratch/make-apt-repo.sh")" != "$(awk -F= '/^make_apt_repo_sha256=/{print $2}' "$manifest")"; then
        printf 'make-apt-repo.sh changed since the manifest; rebuild the debs\n' >&2; exit 1
    fi
}

# --- inner script: publish, serve, install, upgrade ------------------------
run_friend() {
    printf '=== [friend] publish -> add repo -> install -> upgrade (commit %s) ===\n' "$commit"
    verify_manifest
    cat > "$scratch/friend.sh" <<'FRIEND'
set -euo pipefail
exec > >(tee /out/evidence.txt) 2>&1
EXPECT_HELP='documented flow: fetch key -> fingerprint-pin -> /etc/apt/keyrings -> signed-by'

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
keyfpr=$(gpg --list-secret-keys --with-colons | awk -F: '/^fpr:/{print $10; exit}')
echo "[evidence] ephemeral signing key fingerprint: $keyfpr"

repo=/srv/voisu-apt
rm -rf "$repo"; mkdir -p /srv

echo '=== publish v1 ==='
v1deb=$(ls /out/v1/*.deb)
echo "[evidence] v1 deb: $(basename "$v1deb")"
VOISU_APT_GPG_KEY="$keyfpr" /opt/make-apt-repo.sh "$repo" "$v1deb"

echo '=== serve repo over local HTTP ==='
( cd "$repo" && python3 -m http.server 8099 --bind 127.0.0.1 ) >/tmp/http.log 2>&1 &
http_pid=$!
trap 'kill "$http_pid" 2>/dev/null || true' EXIT
for i in $(seq 1 20); do
    curl -fsS http://127.0.0.1:8099/dists/stable/InRelease >/dev/null 2>&1 && break
    sleep 0.5
done

echo "=== add repo the DOCUMENTED way ($EXPECT_HELP) ==="
tmp=$(mktemp -d)
curl -fsSL http://127.0.0.1:8099/voisu-archive-keyring.asc -o "$tmp/key.asc"
got=$(gpg --show-keys --with-colons "$tmp/key.asc" | awk -F: '/^fpr:/{print $10; exit}')
if test "$got" != "$keyfpr"; then
    echo "FAIL: served key fingerprint $got != publisher key $keyfpr"; exit 1
fi
echo "[evidence] served key fingerprint matches publisher: $got"
install -d -m 0755 /etc/apt/keyrings
gpg --dearmor < "$tmp/key.asc" > "$tmp/voisu.gpg"
install -m 0644 "$tmp/voisu.gpg" /etc/apt/keyrings/voisu-archive-keyring.gpg
echo 'deb [signed-by=/etc/apt/keyrings/voisu-archive-keyring.gpg arch=amd64] http://127.0.0.1:8099 stable main' \
    > /etc/apt/sources.list.d/voisu.list
cat /etc/apt/sources.list.d/voisu.list

echo '=== apt-get update (full: needs ubuntu deps too) ==='
apt-get update

echo '=== signature-enforcement NEGATIVE test (a WRONG key must be rejected) ==='
# apt-get update EXITS 0 even on a verification failure (it just warns and keeps
# the old index), so we assert on the emitted error, not the exit code.
wrong_home=$(mktemp -d); chmod 700 "$wrong_home"
GNUPGHOME="$wrong_home" gpg --batch --gen-key "$GNUPGHOME/keyspec" 2>/dev/null
GNUPGHOME="$wrong_home" gpg --batch --yes --export -o /etc/apt/keyrings/voisu-archive-keyring.gpg
apt-get update -o Dir::Etc::sourcelist=/etc/apt/sources.list.d/voisu.list \
    -o Dir::Etc::sourceparts=/dev/null -o APT::Get::List-Cleanup=0 2>&1 | tee /tmp/neg.log || true
if grep -Eqi 'NO_PUBKEY|not signed|signature|not updated' /tmp/neg.log; then
    echo '[evidence] apt REFUSED the repo under the wrong key:'
    grep -Ei 'NO_PUBKEY|not signed|verification|not updated' /tmp/neg.log | head -2
else
    echo 'FAIL: a wrong signing key did not trigger a verification failure'; exit 1
fi
# restore the real (ephemeral) key
install -m 0644 "$tmp/voisu.gpg" /etc/apt/keyrings/voisu-archive-keyring.gpg
apt-get update >/dev/null

echo '=== detached Release.gpg fallback test (InRelease unavailable) ==='
# With InRelease removed, apt MUST authenticate via Release + detached Release.gpg.
mv "$repo/dists/stable/InRelease" "$repo/dists/stable/InRelease.hidden"
fb_lists=$(mktemp -d)
if apt-get update -o Dir::State::lists="$fb_lists" \
        -o Dir::Etc::sourcelist=/etc/apt/sources.list.d/voisu.list \
        -o Dir::Etc::sourceparts=/dev/null -o APT::Get::List-Cleanup=0 2>&1 | tee /tmp/fb.log \
   && ! grep -Eqi 'NO_PUBKEY|not signed|not updated|verification failed' /tmp/fb.log; then
    if grep -q 'Release.gpg' /tmp/fb.log; then
        echo '[evidence] apt authenticated via detached Release.gpg (InRelease absent):'
        grep -E 'Release(\.gpg)?' /tmp/fb.log | head -3
    else
        echo 'FAIL: fallback update did not fetch Release.gpg'; cat /tmp/fb.log; exit 1
    fi
else
    echo 'FAIL: apt could not authenticate via detached Release.gpg'; cat /tmp/fb.log; exit 1
fi
mv "$repo/dists/stable/InRelease.hidden" "$repo/dists/stable/InRelease"

echo '=== install voisu (signature verified) ==='
apt-get update >/dev/null
DEBIAN_FRONTEND=noninteractive apt-get install -y voisu
v1ver=$(dpkg-query -W -f='${Version}' voisu)
echo "[evidence] installed version (v1): $v1ver"

echo '=== republish a strictly-newer v2 (idempotent) ==='
v2deb=$(ls /out/v2/*.deb)
echo "[evidence] v2 deb: $(basename "$v2deb")"
VOISU_APT_GPG_KEY="$keyfpr" /opt/make-apt-repo.sh "$repo" "$v2deb"
echo '[evidence] pool after republish:'
ls -1 "$repo/pool/main/v/voisu"

echo '=== apt-get update + --only-upgrade voisu picks up v2 ==='
apt-get update
DEBIAN_FRONTEND=noninteractive apt-get install --only-upgrade -y voisu
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
        -v "$scratch/make-apt-repo.sh:/opt/make-apt-repo.sh:ro" \
        -v "$scratch/friend.sh:/opt/friend.sh:ro" \
        "$image" bash /opt/friend.sh
}

case "$phase" in
    image)  build_image ;;
    debs)   build_debs ;;
    friend) run_friend ;;
    all)    build_image; build_debs; run_friend ;;
esac
